//! Portal OUTPUT reading (hud-ljhku.2): HTTPS link activation, drag / double /
//! triple-click selection, Ctrl+C, the fenced-block copy button, keyboard
//! stops and the cursor the pointer shows.
//!
//! Pure runtime-local state over the compositor's shaped
//! [`PortalDocumentLayout`]; nothing here reaches an agent. The caller performs
//! the returned [`ReadingEffect`]s (browser handoff, clipboard write) outside
//! scene guards and draws [`OutputReading::decor`].
//!
//! Positions are tile-local pixels; `scroll_y` is the OUTPUT scroll offset.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tze_hud_scene::{PortalCopyButton, PortalDocumentLayout, PortalReadingDecor, Rect, SceneId};

use crate::PortalCursor;

/// Press-to-drag threshold off Windows (`SM_CXDRAG`/`SM_CYDRAG` default).
pub const DEFAULT_DRAG_THRESHOLD_PX: f32 = 4.0;
/// Multi-click interval off Windows (`GetDoubleClickTime` default).
pub const DEFAULT_MULTI_CLICK: Duration = Duration::from_millis(500);
/// The card rate limit: one launch per link per two seconds.
pub const LINK_REPEAT: Duration = Duration::from_secs(2);

/// What lies under the pointer in OUTPUT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadingTarget {
    /// Inside OUTPUT but not on text.
    Blank,
    /// On drawn text at this byte.
    Text(usize),
    /// On link `index`.
    Link(usize),
    /// On the copy button of fenced block `index`.
    CopyButton(usize),
}

/// A keyboard stop inside OUTPUT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadingStop {
    Link(usize),
    CopyButton(usize),
}

/// Work the caller performs after releasing scene guards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadingEffect {
    /// Hand this validated HTTPS URL to the link worker.
    Open(String),
    /// Write this plain text to the clipboard.
    Copy(String),
}

/// An OUTPUT selection, anchored to plain-text bytes of the current layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub tile: SceneId,
    pub anchor: usize,
    pub head: usize,
    /// The view was following the tail when the selection began.
    pub resume_tail: bool,
}

impl Selection {
    pub fn range(&self) -> (usize, usize) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }
}

#[derive(Clone, Copy, Debug)]
struct Press {
    tile: SceneId,
    origin: (f32, f32),
    byte: usize,
    target: ReadingTarget,
    clicks: u8,
    dragging: bool,
}

/// Outcome of a pointer event, for the caller's focus and repaint decisions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadingOutcome {
    /// The event was an OUTPUT reading interaction (consume it).
    pub handled: bool,
    /// A selection now exists that did not before (take keyboard focus).
    pub selection_started: bool,
    /// A press cleared this selection (resume the tail if it began there).
    pub cleared: Option<Selection>,
    pub effect: Option<ReadingEffect>,
}

/// All OUTPUT reading state for one runtime.
#[derive(Debug)]
pub struct OutputReading {
    pub selection: Option<Selection>,
    pub focus: Option<(SceneId, ReadingStop)>,
    hover: Option<(SceneId, ReadingTarget, (f32, f32))>,
    press: Option<Press>,
    last_click: Option<(Instant, (f32, f32), SceneId, u8)>,
    launched: HashMap<(SceneId, Arc<str>), Instant>,
    copied: Option<(SceneId, usize)>,
    pub drag_threshold_px: f32,
    pub multi_click: Duration,
}

impl Default for OutputReading {
    fn default() -> Self {
        Self {
            selection: None,
            focus: None,
            hover: None,
            press: None,
            last_click: None,
            launched: HashMap::new(),
            copied: None,
            drag_threshold_px: DEFAULT_DRAG_THRESHOLD_PX,
            multi_click: DEFAULT_MULTI_CLICK,
        }
    }
}

/// Document coordinates of a tile-local point, `None` outside the viewport.
fn doc_point(
    layout: &PortalDocumentLayout,
    scroll_y: f32,
    (x, y): (f32, f32),
) -> Option<(f32, f32)> {
    let v = layout.viewport;
    (x >= v.x && x < v.x + v.width && y >= v.y && y < v.y + v.height)
        .then_some((x - v.x, y - v.y + scroll_y))
}

/// The block whose rows contain document offset `y`.
fn block_at_y(layout: &PortalDocumentLayout, y: f32) -> Option<usize> {
    let row = layout.row_at(y)?;
    let start = layout.rows[row].0;
    let end = layout
        .rows
        .get(row + 1)
        .map_or(layout.content.len(), |r| r.0);
    layout
        .reading
        .code_blocks
        .iter()
        .position(|&(s, e)| s < end.max(start + 1) && e >= start)
}

/// Hit-test OUTPUT: copy button, then link, then text, else blank (`None`
/// outside the viewport).
pub fn target_at(
    layout: &PortalDocumentLayout,
    scroll_y: f32,
    p: (f32, f32),
) -> Option<ReadingTarget> {
    let (x, y) = doc_point(layout, scroll_y, p)?;
    for i in 0..layout.reading.code_blocks.len() {
        if layout
            .copy_button_rect(i, scroll_y)
            .is_some_and(|r| r.contains_point(p.0, p.1))
        {
            return Some(ReadingTarget::CopyButton(i));
        }
    }
    Some(match layout.glyph_at(x, y) {
        Some(byte) => layout
            .link_at(byte)
            .map_or(ReadingTarget::Text(byte), ReadingTarget::Link),
        None => ReadingTarget::Blank,
    })
}

/// Cursor for an OUTPUT target: I-beam over text, hand over links and buttons.
pub fn reading_cursor(target: Option<ReadingTarget>) -> PortalCursor {
    match target {
        Some(ReadingTarget::Text(_)) => PortalCursor::Text,
        Some(ReadingTarget::Link(_) | ReadingTarget::CopyButton(_)) => PortalCursor::Pointer,
        Some(ReadingTarget::Blank) | None => PortalCursor::Default,
    }
}

impl OutputReading {
    /// Selected plain text of `tile`, if any.
    pub fn selected_text(&self, tile: SceneId, layout: &PortalDocumentLayout) -> Option<String> {
        let sel = self.selection.filter(|s| s.tile == tile)?;
        let (s, e) = sel.range();
        layout
            .content
            .get(s..e)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    }

    /// Characters selected (0 when none).
    pub fn selection_chars(&self, tile: SceneId, layout: &PortalDocumentLayout) -> usize {
        self.selected_text(tile, layout)
            .map_or(0, |t| t.chars().count())
    }

    /// The portal whose OUTPUT the pointer is over.
    pub fn hover_tile(&self) -> Option<SceneId> {
        self.hover.map(|h| h.0)
    }

    /// Current hover target of `tile`.
    pub fn hover(&self, tile: SceneId) -> Option<ReadingTarget> {
        self.hover.filter(|h| h.0 == tile).map(|h| h.1)
    }

    /// Cursor while the pointer is over `tile`'s OUTPUT (I-beam while dragging).
    pub fn cursor(&self, tile: SceneId) -> PortalCursor {
        if self.press.is_some_and(|p| p.tile == tile && p.dragging) {
            return PortalCursor::Text;
        }
        reading_cursor(self.hover(tile))
    }

    fn admit_launch(&mut self, tile: SceneId, href: &Arc<str>, now: Instant) -> bool {
        let key = (tile, href.clone());
        if self
            .launched
            .get(&key)
            .is_some_and(|t| now.saturating_duration_since(*t) < LINK_REPEAT)
        {
            return false;
        }
        self.launched
            .retain(|_, t| now.saturating_duration_since(*t) < LINK_REPEAT);
        self.launched.insert(key, now);
        true
    }

    fn open(
        &mut self,
        tile: SceneId,
        layout: &PortalDocumentLayout,
        link: usize,
        now: Instant,
    ) -> Option<ReadingEffect> {
        let href = layout.reading.links.get(link)?.href.clone();
        self.admit_launch(tile, &href, now)
            .then(|| ReadingEffect::Open(href.to_string()))
    }

    fn copy_block(
        &mut self,
        tile: SceneId,
        layout: &PortalDocumentLayout,
        block: usize,
    ) -> Option<ReadingEffect> {
        let (s, e) = *layout.reading.code_blocks.get(block)?;
        self.copied = Some((tile, block));
        layout
            .content
            .get(s..e)
            .map(|t| ReadingEffect::Copy(t.to_owned()))
    }

    /// Track hover without a press (cursor, underline, hint, copy button).
    pub fn pointer_hover(
        &mut self,
        tile: Option<SceneId>,
        layout: Option<&PortalDocumentLayout>,
        scroll_y: f32,
        p: (f32, f32),
    ) {
        self.hover = tile
            .zip(layout)
            .and_then(|(t, l)| Some((t, target_at(l, scroll_y, p)?, p)));
        if self.copied.is_some_and(|(t, b)| {
            !self.button_shown(t, b, layout.filter(|_| tile == Some(t)), scroll_y)
        }) {
            self.copied = None;
        }
    }

    fn button_shown(
        &self,
        tile: SceneId,
        block: usize,
        layout: Option<&PortalDocumentLayout>,
        scroll_y: f32,
    ) -> bool {
        self.focus == Some((tile, ReadingStop::CopyButton(block)))
            || layout
                .zip(self.hover.filter(|h| h.0 == tile))
                .is_some_and(|(l, h)| {
                    doc_point(l, scroll_y, h.2).and_then(|(_, y)| block_at_y(l, y)) == Some(block)
                })
    }

    /// Primary press inside OUTPUT. `None` target (outside) ends nothing.
    pub fn pointer_down(
        &mut self,
        tile: SceneId,
        layout: &PortalDocumentLayout,
        scroll_y: f32,
        p: (f32, f32),
        now: Instant,
        at_tail: bool,
    ) -> ReadingOutcome {
        let Some(target) = target_at(layout, scroll_y, p) else {
            return ReadingOutcome::default();
        };
        self.pointer_hover(Some(tile), Some(layout), scroll_y, p);
        let clicks = match self.last_click {
            Some((t, at, last_tile, n))
                if last_tile == tile
                    && now.saturating_duration_since(t) <= self.multi_click
                    && (at.0 - p.0).abs() <= self.drag_threshold_px
                    && (at.1 - p.1).abs() <= self.drag_threshold_px =>
            {
                n % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, p, tile, clicks));
        let (dx, dy) = doc_point(layout, scroll_y, p).unwrap_or_default();
        let byte = layout.byte_at(dx, dy);
        self.press = Some(Press {
            tile,
            origin: p,
            byte,
            target,
            clicks,
            dragging: false,
        });
        let mut out = ReadingOutcome {
            handled: true,
            ..Default::default()
        };
        if matches!(target, ReadingTarget::CopyButton(_)) {
            return out;
        }
        let old = self.selection.take();
        let had = old.is_some();
        let resume_tail = old.map_or(at_tail, |s| s.resume_tail);
        if !matches!(target, ReadingTarget::Link(_)) && clicks >= 2 {
            let (s, e) = if clicks == 2 {
                layout.word_at(byte)
            } else {
                layout.paragraph_at(byte)
            };
            if s < e {
                self.selection = Some(Selection {
                    tile,
                    anchor: s,
                    head: e,
                    resume_tail,
                });
                out.selection_started = !had;
            }
        }
        if self.selection.is_none() {
            out.cleared = old;
        }
        out
    }

    /// No selection, focus, hover or press: nothing to draw.
    pub fn is_idle(&self) -> bool {
        self.selection.is_none()
            && self.focus.is_none()
            && self.hover.is_none()
            && self.press.is_none()
    }

    /// The OUTPUT tile a press began on, while it is held.
    pub fn press_tile(&self) -> Option<SceneId> {
        self.press.map(|p| p.tile)
    }

    /// Pointer motion while pressed; past the drag threshold a press selects
    /// (and a link press no longer opens). Returns whether it was handled.
    pub fn pointer_move(
        &mut self,
        layout: Option<&PortalDocumentLayout>,
        scroll_y: f32,
        p: (f32, f32),
        at_tail: bool,
    ) -> ReadingOutcome {
        let Some(mut press) = self.press else {
            return ReadingOutcome::default();
        };
        let mut out = ReadingOutcome {
            handled: true,
            ..Default::default()
        };
        if !press.dragging
            && ((press.origin.0 - p.0).abs() > self.drag_threshold_px
                || (press.origin.1 - p.1).abs() > self.drag_threshold_px)
            && !matches!(press.target, ReadingTarget::CopyButton(_))
            && press.clicks == 1
        {
            press.dragging = true;
        }
        if press.dragging
            && let Some(layout) = layout
        {
            let v = layout.viewport;
            let x = (p.0 - v.x).clamp(0.0, v.width);
            let y = (p.1 - v.y).clamp(0.0, v.height) + scroll_y;
            let head = layout.byte_at(x, y);
            let had = self.selection.is_some();
            let resume_tail = self.selection.map_or(at_tail, |s| s.resume_tail);
            self.selection = (head != press.byte).then_some(Selection {
                tile: press.tile,
                anchor: press.byte,
                head,
                resume_tail,
            });
            out.selection_started = !had && self.selection.is_some();
        }
        self.press = Some(press);
        out
    }

    /// Primary release: a link opens on the first release of a press that
    /// stayed within the threshold; the copy button copies on release over it.
    pub fn pointer_up(
        &mut self,
        layout: Option<&PortalDocumentLayout>,
        scroll_y: f32,
        p: (f32, f32),
        now: Instant,
    ) -> ReadingOutcome {
        let Some(press) = self.press.take() else {
            return ReadingOutcome::default();
        };
        let mut out = ReadingOutcome {
            handled: true,
            ..Default::default()
        };
        let Some(layout) = layout else {
            return out;
        };
        let released = target_at(layout, scroll_y, p);
        match press.target {
            ReadingTarget::Link(i) if !press.dragging && press.clicks == 1 => {
                out.effect = self.open(press.tile, layout, i, now);
            }
            ReadingTarget::CopyButton(i) if released == Some(press.target) => {
                out.effect = self.copy_block(press.tile, layout, i);
            }
            _ => {}
        }
        out
    }

    /// Drop a press whose window, tile or capture went away.
    pub fn cancel_press(&mut self) {
        self.press = None;
    }

    /// Ctrl+C: the selected plain text.
    pub fn copy(&self, tile: SceneId, layout: &PortalDocumentLayout) -> Option<ReadingEffect> {
        self.selected_text(tile, layout).map(ReadingEffect::Copy)
    }

    /// Esc, innermost first: the selection, then a reading stop. Returns the
    /// cleared selection (its `resume_tail`) or `None` when nothing cleared.
    pub fn escape(&mut self) -> Option<Option<Selection>> {
        if let Some(sel) = self.selection.take() {
            return Some(Some(sel));
        }
        self.focus.take().map(|_| None)
    }

    /// Keyboard stops of OUTPUT visible at `scroll_y`, in reading order.
    pub fn stops(layout: &PortalDocumentLayout, scroll_y: f32) -> Vec<ReadingStop> {
        let visible = |byte: usize| {
            layout
                .rows
                .iter()
                .rposition(|(b, _)| *b <= byte)
                .is_some_and(|r| {
                    let top = layout.rows[r].1 - scroll_y;
                    top >= 0.0 && top + layout.line_height <= layout.viewport.height + 0.5
                })
        };
        let mut stops: Vec<(usize, ReadingStop)> = layout
            .reading
            .links
            .iter()
            .enumerate()
            .filter(|(_, l)| visible(l.start))
            .map(|(i, l)| (l.start, ReadingStop::Link(i)))
            .collect();
        stops.extend(
            (0..layout.reading.code_blocks.len())
                .filter(|&i| layout.copy_button_rect(i, scroll_y).is_some())
                .map(|i| (layout.reading.code_blocks[i].0, ReadingStop::CopyButton(i))),
        );
        stops.sort_by_key(|(b, _)| *b);
        stops.into_iter().map(|(_, s)| s).collect()
    }

    /// Tab / Shift+Tab inside OUTPUT. `entering` allows a forward Tab with no
    /// current stop to enter the first one. Returns `false` when the move
    /// leaves OUTPUT (the caller continues ordinary traversal).
    pub fn tab(
        &mut self,
        tile: SceneId,
        layout: &PortalDocumentLayout,
        scroll_y: f32,
        reverse: bool,
        entering: bool,
    ) -> bool {
        let stops = Self::stops(layout, scroll_y);
        let current = self
            .focus
            .filter(|f| f.0 == tile)
            .and_then(|f| stops.iter().position(|s| *s == f.1));
        let next = match (current, reverse) {
            (None, false) if entering => stops.first().copied(),
            (None, _) => None,
            (Some(i), false) => stops.get(i + 1).copied(),
            (Some(i), true) => i.checked_sub(1).map(|i| stops[i]),
        };
        self.copied = None;
        self.focus = next.map(|s| (tile, s));
        next.is_some()
    }

    /// Enter on a focused stop.
    pub fn activate(
        &mut self,
        tile: SceneId,
        layout: &PortalDocumentLayout,
        now: Instant,
    ) -> Option<ReadingEffect> {
        match self.focus.filter(|f| f.0 == tile)?.1 {
            ReadingStop::Link(i) => self.open(tile, layout, i, now),
            ReadingStop::CopyButton(i) => self.copy_block(tile, layout, i),
        }
    }

    /// Carry the selection and stops across a reshaped document; returns the
    /// selection if it was dropped because its text left the transcript.
    pub fn remap(
        &mut self,
        tile: SceneId,
        old: &PortalDocumentLayout,
        new: &PortalDocumentLayout,
    ) -> Option<Selection> {
        if old.content != new.content {
            if self.focus.is_some_and(|f| f.0 == tile) {
                self.focus = None;
            }
            if self.copied.is_some_and(|c| c.0 == tile) {
                self.copied = None;
            }
        }
        let sel = self.selection.filter(|s| s.tile == tile)?;
        match old.remap_range(new, sel.range()) {
            Some((anchor, head)) => {
                self.selection = Some(Selection {
                    anchor,
                    head,
                    ..sel
                });
                None
            }
            None => self.selection.take(),
        }
    }

    /// Forget everything about tiles that no longer show OUTPUT (cleared,
    /// reclaimed, dismissed or owner-hidden). Returns a dropped selection.
    pub fn retain(&mut self, mut live: impl FnMut(SceneId) -> bool) -> Option<Selection> {
        if self.focus.is_some_and(|f| !live(f.0)) {
            self.focus = None;
        }
        if self.hover.is_some_and(|h| !live(h.0)) {
            self.hover = None;
        }
        if self.press.is_some_and(|p| !live(p.tile)) {
            self.press = None;
        }
        if self.copied.is_some_and(|c| !live(c.0)) {
            self.copied = None;
        }
        self.launched.retain(|k, _| live(k.0));
        self.selection
            .filter(|s| !live(s.tile))
            .and_then(|_| self.selection.take())
    }

    /// The link whose underline and hint show: keyboard focus, else hover.
    pub fn shown_link(&self, tile: SceneId) -> Option<usize> {
        match (self.focus.filter(|f| f.0 == tile), self.hover(tile)) {
            (Some((_, ReadingStop::Link(i))), _) => Some(i),
            (_, Some(ReadingTarget::Link(i))) => Some(i),
            _ => None,
        }
    }

    /// Host the hover hint shows for `tile` (the admin `link_hint`).
    pub fn link_hint(&self, tile: SceneId, layout: &PortalDocumentLayout) -> Option<String> {
        layout.reading.links.get(self.shown_link(tile)?)?.host()
    }

    /// What the compositor draws for `tile`.
    pub fn decor(
        &self,
        tile: SceneId,
        layout: &PortalDocumentLayout,
        scroll_y: f32,
    ) -> PortalReadingDecor {
        let selection = self.selection.filter(|s| s.tile == tile).map(|s| s.range());
        let link = self
            .shown_link(tile)
            .and_then(|i| layout.reading.links.get(i));
        let hint = link.and_then(|l| {
            let host = l.host()?;
            let last = *layout.range_rects(l.start, l.end).last()?;
            let v = layout.viewport;
            let h = layout.line_height.max(1.0);
            // Estimated label width: about 0.6 em per character plus padding.
            let w = (h * (0.45 * host.chars().count() as f32 + 1.0)).min(v.width);
            let below = v.y + last.y + last.height - scroll_y;
            let y = if below + h <= v.y + v.height {
                below
            } else {
                below - last.height - h
            };
            let x = (v.x + last.x).min(v.x + v.width - w).max(v.x);
            Some((host, Rect::new(x, y, w, h)))
        });
        let copy_button = (0..layout.reading.code_blocks.len())
            .find(|&b| self.button_shown(tile, b, Some(layout), scroll_y))
            .and_then(|block| {
                Some(PortalCopyButton {
                    block,
                    bounds: layout.copy_button_rect(block, scroll_y)?,
                    copied: self.copied == Some((tile, block)),
                    focused: self.focus == Some((tile, ReadingStop::CopyButton(block))),
                })
            });
        PortalReadingDecor {
            selection,
            link: link.map(|l| (l.start, l.end)),
            hint,
            copy_button,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::types::{FontFamily, TextAlign, TextOverflow};
    use tze_hud_scene::{PortalGlyph, PortalLink, PortalReadingLayout, Rgba, TextMarkdownNode};

    /// Monospace fixture: 10 px per byte, 20 px rows, viewport at the tile origin.
    fn layout(text: &str) -> PortalDocumentLayout {
        let (mut rows, mut glyphs, mut row_glyphs, mut start) = (vec![], vec![], vec![], 0);
        for (i, line) in text.split('\n').enumerate() {
            rows.push((start, i as f32 * 20.0));
            row_glyphs.push(glyphs.len() as u32);
            for (j, c) in line.char_indices() {
                let b = (start + j) as u32;
                glyphs.push(PortalGlyph {
                    start: b,
                    end: b + c.len_utf8() as u32,
                    x: j as f32 * 10.0,
                    w: 10.0,
                });
            }
            start += line.len() + 1;
        }
        PortalDocumentLayout {
            node_id: SceneId::new(),
            signature: [0; 32],
            source: TextMarkdownNode {
                content: text.into(),
                bounds: Rect::new(0.0, 0.0, 400.0, 200.0),
                font_size_px: 14.0,
                font_family: FontFamily::SystemSansSerif,
                color: Rgba::WHITE,
                background: None,
                alignment: TextAlign::Start,
                overflow: TextOverflow::Clip,
                color_runs: Box::new([]),
            },
            tile_size: (400.0, 200.0),
            font_scale: 1.0,
            content: text.into(),
            height: rows.len() as f32 * 20.0,
            viewport: Rect::new(0.0, 0.0, 400.0, 200.0),
            line_height: 20.0,
            rows: rows.into(),
            reading: PortalReadingLayout {
                glyphs: glyphs.into(),
                row_glyphs: row_glyphs.into(),
                ..Default::default()
            },
        }
    }

    /// "see docs here" with "docs" linked, then two more lines and a block.
    fn doc() -> PortalDocumentLayout {
        let mut l = layout("see docs here\nline two\nline three\n\nls -la");
        l.reading.links = vec![PortalLink {
            start: 4,
            end: 8,
            href: "https://example.com/a".into(),
        }]
        .into();
        l.reading.code_blocks = vec![(35, 41)].into();
        l
    }

    const LINK: (f32, f32) = (55.0, 5.0);

    fn click(
        r: &mut OutputReading,
        t: SceneId,
        l: &PortalDocumentLayout,
        p: (f32, f32),
        now: Instant,
    ) -> ReadingOutcome {
        r.pointer_down(t, l, 0.0, p, now, true);
        r.pointer_up(Some(l), 0.0, p, now)
    }

    #[test]
    fn link_click_within_threshold_opens_once_on_release() {
        let (mut r, t, l, now) = (
            OutputReading::default(),
            SceneId::new(),
            doc(),
            Instant::now(),
        );
        let down = r.pointer_down(t, &l, 0.0, LINK, now, true);
        assert_eq!(down.effect, None, "nothing opens on press");
        r.pointer_move(Some(&l), 0.0, (LINK.0 + 3.0, LINK.1 + 3.0), true);
        let up = r.pointer_up(Some(&l), 0.0, (LINK.0 + 3.0, LINK.1 + 3.0), now);
        assert_eq!(
            up.effect,
            Some(ReadingEffect::Open("https://example.com/a".into()))
        );
        assert!(r.selection.is_none());
    }

    #[test]
    fn link_press_past_threshold_selects_and_opens_nothing() {
        let (mut r, t, l, now) = (
            OutputReading::default(),
            SceneId::new(),
            doc(),
            Instant::now(),
        );
        r.pointer_down(t, &l, 0.0, LINK, now, true);
        let moved = r.pointer_move(Some(&l), 0.0, (LINK.0 + 30.0, LINK.1), true);
        assert!(moved.selection_started);
        assert_eq!(
            r.pointer_up(Some(&l), 0.0, (LINK.0 + 30.0, LINK.1), now)
                .effect,
            None
        );
        assert_eq!(r.selected_text(t, &l).as_deref(), Some("cs "));
    }

    #[test]
    fn double_click_on_a_link_opens_once_and_selects_nothing() {
        let (mut r, t, l, now) = (
            OutputReading::default(),
            SceneId::new(),
            doc(),
            Instant::now(),
        );
        assert!(click(&mut r, t, &l, LINK, now).effect.is_some());
        let second = click(&mut r, t, &l, LINK, now + Duration::from_millis(150));
        assert_eq!(second.effect, None);
        assert!(r.selection.is_none());
    }

    #[test]
    fn same_href_within_two_seconds_launches_once() {
        let (mut r, t, l, now) = (
            OutputReading::default(),
            SceneId::new(),
            doc(),
            Instant::now(),
        );
        assert!(click(&mut r, t, &l, LINK, now).effect.is_some());
        let later = now + Duration::from_millis(1_500);
        assert_eq!(
            click(&mut r, t, &l, LINK, later).effect,
            None,
            "rate limited"
        );
        let after = now + Duration::from_millis(2_100);
        assert!(click(&mut r, t, &l, LINK, after).effect.is_some());
    }

    #[test]
    fn double_and_triple_click_select_word_and_paragraph() {
        let (mut r, t, l, now) = (
            OutputReading::default(),
            SceneId::new(),
            doc(),
            Instant::now(),
        );
        let word = (25.0, 25.0); // "line two" → "line"
        click(&mut r, t, &l, word, now);
        let d = r.pointer_down(t, &l, 0.0, word, now + Duration::from_millis(100), true);
        assert!(d.selection_started);
        assert_eq!(r.selected_text(t, &l).as_deref(), Some("line"));
        r.pointer_up(Some(&l), 0.0, word, now);
        r.pointer_down(t, &l, 0.0, word, now + Duration::from_millis(200), true);
        assert_eq!(
            r.selected_text(t, &l).as_deref(),
            Some("see docs here\nline two\nline three")
        );
    }

    #[test]
    fn drag_across_three_lines_copies_exactly_that_text() {
        let (mut r, t, l, now) = (
            OutputReading::default(),
            SceneId::new(),
            doc(),
            Instant::now(),
        );
        r.pointer_down(t, &l, 0.0, (0.0, 5.0), now, true);
        r.pointer_move(Some(&l), 0.0, (40.0, 45.0), true);
        r.pointer_up(Some(&l), 0.0, (40.0, 45.0), now);
        assert_eq!(
            r.copy(t, &l),
            Some(ReadingEffect::Copy("see docs here\nline two\nline".into()))
        );
        assert_eq!(r.selection_chars(t, &l), 27);
    }

    #[test]
    fn selection_follows_its_text_and_clears_when_evicted() {
        let (mut r, t, l, now) = (
            OutputReading::default(),
            SceneId::new(),
            doc(),
            Instant::now(),
        );
        r.pointer_down(t, &l, 0.0, (2.0, 25.0), now, true);
        r.pointer_move(Some(&l), 0.0, (82.0, 25.0), true);
        let appended = layout("see docs here\nline two\nline three\n\nls -la\nmore");
        assert_eq!(r.remap(t, &l, &appended), None);
        assert_eq!(r.selected_text(t, &appended).as_deref(), Some("line two"));
        let trimmed = layout("line two\nline three");
        assert_eq!(r.remap(t, &appended, &trimmed), None);
        assert_eq!(r.selected_text(t, &trimmed).as_deref(), Some("line two"));
        let evicted = layout("line three\nnew");
        let dropped = r.remap(t, &trimmed, &evicted).expect("dropped");
        assert!(dropped.resume_tail, "began at the tail");
        assert_eq!(r.selection_chars(t, &evicted), 0);
    }

    #[test]
    fn escape_clears_selection_then_focus_and_retain_clears_dead_tiles() {
        let (mut r, t, l, now) = (
            OutputReading::default(),
            SceneId::new(),
            doc(),
            Instant::now(),
        );
        r.pointer_down(t, &l, 0.0, (2.0, 25.0), now, false);
        r.pointer_move(Some(&l), 0.0, (82.0, 25.0), false);
        assert!(r.tab(t, &l, 0.0, false, true));
        assert_eq!(
            r.escape(),
            Some(Some(Selection {
                tile: t,
                anchor: 14,
                head: 22,
                resume_tail: false
            }))
        );
        assert_eq!(r.escape(), Some(None), "then the focused stop");
        assert_eq!(r.escape(), None, "nothing left: the outer Esc proceeds");
        r.pointer_down(t, &l, 0.0, (2.0, 25.0), now, false);
        r.pointer_move(Some(&l), 0.0, (82.0, 25.0), false);
        assert!(
            r.retain(|id| id != t).is_some(),
            "cleared/reclaimed/hidden tile"
        );
        assert!(r.selection.is_none());
    }

    #[test]
    fn tab_walks_visible_links_then_copy_buttons_and_enter_activates() {
        let (mut r, t, l, now) = (
            OutputReading::default(),
            SceneId::new(),
            doc(),
            Instant::now(),
        );
        assert!(
            !r.tab(t, &l, 0.0, false, false),
            "no entry without the composer origin"
        );
        assert!(r.tab(t, &l, 0.0, false, true));
        assert_eq!(r.focus, Some((t, ReadingStop::Link(0))));
        assert_eq!(
            r.activate(t, &l, now),
            Some(ReadingEffect::Open("https://example.com/a".into()))
        );
        assert_eq!(r.link_hint(t, &l).as_deref(), Some("example.com"));
        assert!(r.tab(t, &l, 0.0, false, false));
        assert_eq!(
            r.activate(t, &l, now),
            Some(ReadingEffect::Copy("ls -la".into()))
        );
        assert!(
            r.decor(t, &l, 0.0)
                .copy_button
                .is_some_and(|b| b.copied && b.focused)
        );
        assert!(!r.tab(t, &l, 0.0, false, false), "leaving OUTPUT");
        assert!(r.focus.is_none());
    }

    #[test]
    fn copy_button_shows_on_hover_and_copies_the_block_verbatim() {
        let (mut r, t, l, now) = (
            OutputReading::default(),
            SceneId::new(),
            doc(),
            Instant::now(),
        );
        r.pointer_hover(Some(t), Some(&l), 0.0, (5.0, 85.0));
        let button = r.decor(t, &l, 0.0).copy_button.expect("hovering the block");
        assert_eq!(button.bounds, Rect::new(320.0, 80.0, 80.0, 20.0));
        let at = (330.0, 85.0);
        assert_eq!(
            reading_cursor(target_at(&l, 0.0, at)),
            PortalCursor::Pointer
        );
        assert_eq!(
            click(&mut r, t, &l, at, now).effect,
            Some(ReadingEffect::Copy("ls -la".into()))
        );
        assert!(r.decor(t, &l, 0.0).copy_button.unwrap().copied);
        r.pointer_hover(Some(t), Some(&l), 0.0, (5.0, 5.0));
        assert!(
            r.decor(t, &l, 0.0).copy_button.is_none(),
            "Copied ends when the button hides"
        );
    }

    #[test]
    fn cursor_is_ibeam_over_text_hand_over_links_arrow_elsewhere() {
        let l = doc();
        assert_eq!(
            reading_cursor(target_at(&l, 0.0, (5.0, 25.0))),
            PortalCursor::Text
        );
        assert_eq!(
            reading_cursor(target_at(&l, 0.0, LINK)),
            PortalCursor::Pointer
        );
        assert_eq!(
            reading_cursor(target_at(&l, 0.0, (300.0, 25.0))),
            PortalCursor::Default
        );
        assert_eq!(
            reading_cursor(target_at(&l, 0.0, (500.0, 25.0))),
            PortalCursor::Default
        );
    }

    #[test]
    fn hint_shows_the_href_host_under_the_link_only_while_hovered() {
        let (mut r, t, l) = (OutputReading::default(), SceneId::new(), doc());
        r.pointer_hover(Some(t), Some(&l), 0.0, LINK);
        let decor = r.decor(t, &l, 0.0);
        assert_eq!(decor.link, Some((4, 8)));
        let (host, rect) = decor.hint.expect("hint");
        assert_eq!(host, "example.com");
        assert_eq!(rect.y, 20.0, "below the link row");
        assert_eq!(r.link_hint(t, &l).as_deref(), Some("example.com"));
        r.pointer_hover(Some(t), Some(&l), 0.0, (5.0, 25.0));
        assert_eq!(r.link_hint(t, &l), None);
    }
}
