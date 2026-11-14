//! Portal reading geometry (hud-ljhku.2): links, glyph extents and fenced code
//! blocks of a shaped OUTPUT document, plus the runtime decoration the
//! compositor draws for selection, link hover/focus and the code copy button.
//!
//! All byte offsets index the document's plain text
//! ([`PortalDocumentLayout::content`]); x is relative to the viewport's left
//! edge and y is a document offset (subtract the scroll offset for the view).

use std::sync::Arc;

use crate::types::{PortalDocumentLayout, Rect};

/// Maximum accepted link target length, shared with notification url actions.
pub const MAX_LINK_BYTES: usize = 2048;

/// The card URL rule: HTTPS with a non-empty host and at most 2048 bytes.
/// Returns the parser's serialized form, the only form that may leave the scene.
pub fn https_link_target(raw: &str) -> Option<String> {
    if raw.len() > MAX_LINK_BYTES {
        return None;
    }
    let parsed = url::Url::parse(raw).ok()?;
    (parsed.scheme() == "https" && parsed.host_str().is_some_and(|h| !h.is_empty()))
        .then(|| parsed.into())
}

/// A link in the plain text, `[start, end)`, with its validated HTTPS target.
#[derive(Clone, Debug, PartialEq)]
pub struct PortalLink {
    pub start: usize,
    pub end: usize,
    pub href: Arc<str>,
}

impl PortalLink {
    /// The host the hover hint shows (markdown text can differ from the href).
    pub fn host(&self) -> Option<String> {
        url::Url::parse(&self.href)
            .ok()?
            .host_str()
            .map(str::to_owned)
    }
}

/// One shaped glyph: its plain-text bytes and horizontal extent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PortalGlyph {
    pub start: u32,
    pub end: u32,
    pub x: f32,
    pub w: f32,
}

/// Reading geometry published beside the row table.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PortalReadingLayout {
    pub links: Arc<[PortalLink]>,
    /// Fenced code block contents, `[start, end)` without the trailing newline.
    pub code_blocks: Arc<[(usize, usize)]>,
    /// Glyphs in row order; row `i` owns `glyphs[row_glyphs[i]..row_glyphs[i + 1]]`.
    pub glyphs: Arc<[PortalGlyph]>,
    pub row_glyphs: Arc<[u32]>,
}

/// What the compositor draws for one portal's OUTPUT reading state. Runtime
/// state only; a change repaints, never reshapes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PortalReadingDecor {
    /// Selected plain-text range.
    pub selection: Option<(usize, usize)>,
    /// Hovered or keyboard-focused link range (underlined).
    pub link: Option<(usize, usize)>,
    /// The link target host and its tile-local hint anchor (below the link).
    pub hint: Option<(String, Rect)>,
    /// The visible code copy button, if any.
    pub copy_button: Option<PortalCopyButton>,
}

/// A fenced block's copy button in tile-local pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct PortalCopyButton {
    pub block: usize,
    pub bounds: Rect,
    pub copied: bool,
    pub focused: bool,
}

impl PortalDocumentLayout {
    fn row_end(&self, row: usize) -> usize {
        self.rows
            .get(row + 1)
            .map_or(self.content.len(), |r| r.0)
            .min(self.content.len())
    }

    fn row_bottom(&self, row: usize) -> f32 {
        self.rows.get(row + 1).map_or(self.height, |r| r.1)
    }

    fn row_glyphs(&self, row: usize) -> &[PortalGlyph] {
        let r = &self.reading;
        let start = r
            .row_glyphs
            .get(row)
            .map_or(r.glyphs.len(), |&i| i as usize);
        let end = r
            .row_glyphs
            .get(row + 1)
            .map_or(r.glyphs.len(), |&i| i as usize);
        &r.glyphs[start.min(end)..end]
    }

    /// The row at document offset `y`, clamped to the first/last row.
    pub fn row_at(&self, y: f32) -> Option<usize> {
        (!self.rows.is_empty()).then(|| {
            self.rows
                .partition_point(|(_, top)| *top <= y)
                .saturating_sub(1)
        })
    }

    /// Caret byte nearest `(x, y)`: before a glyph's centre is its start,
    /// after the row's last glyph is the row end (before its newline).
    pub fn byte_at(&self, x: f32, y: f32) -> usize {
        let Some(row) = self.row_at(y) else {
            return 0;
        };
        let glyphs = self.row_glyphs(row);
        if let Some(g) = glyphs.iter().find(|g| x < g.x + g.w / 2.0) {
            return g.start as usize;
        }
        let end = self.row_end(row);
        let line_end = self.content[self.rows[row].0.min(end)..end]
            .strip_suffix('\n')
            .map_or(end, |s| self.rows[row].0 + s.len());
        glyphs
            .iter()
            .map(|g| g.end as usize)
            .max()
            .unwrap_or(line_end)
            .max(line_end.min(end))
    }

    /// The glyph's start byte when `(x, y)` lies on drawn text.
    pub fn glyph_at(&self, x: f32, y: f32) -> Option<usize> {
        let row = self.row_at(y)?;
        if y < self.rows[row].1 || y >= self.row_bottom(row) {
            return None;
        }
        self.row_glyphs(row)
            .iter()
            .find(|g| x >= g.x && x < g.x + g.w)
            .map(|g| g.start as usize)
    }

    /// Index of the link covering `byte`.
    pub fn link_at(&self, byte: usize) -> Option<usize> {
        self.reading
            .links
            .iter()
            .position(|l| (l.start..l.end).contains(&byte))
    }

    /// Document-space rectangles covering `[start, end)`, one per row.
    pub fn range_rects(&self, start: usize, end: usize) -> Vec<Rect> {
        let mut out = Vec::new();
        for row in 0..self.rows.len() {
            let (top, bottom) = (self.rows[row].1, self.row_bottom(row));
            let hits: Vec<_> = self
                .row_glyphs(row)
                .iter()
                .filter(|g| (g.start as usize) < end && (g.end as usize) > start)
                .collect();
            let left = hits.iter().map(|g| g.x).fold(f32::INFINITY, f32::min);
            let right = hits.iter().map(|g| g.x + g.w).fold(0.0, f32::max);
            if left.is_finite() && right > left {
                out.push(Rect::new(left, top, right - left, bottom - top));
            }
        }
        out
    }

    /// The word around `byte`; on whitespace or punctuation, that one character.
    pub fn word_at(&self, byte: usize) -> (usize, usize) {
        let text = &self.content;
        let byte = floor_char(text, byte.min(text.len()));
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        match text[byte..].chars().next() {
            Some(c) if is_word(c) => {
                let start = text[..byte]
                    .char_indices()
                    .rev()
                    .take_while(|(_, c)| is_word(*c))
                    .last()
                    .map_or(byte, |(i, _)| i);
                let end = text[byte..]
                    .char_indices()
                    .find(|(_, c)| !is_word(*c))
                    .map_or(text.len(), |(i, _)| byte + i);
                (start, end)
            }
            Some(c) if c != '\n' => (byte, byte + c.len_utf8()),
            _ => (byte, byte),
        }
    }

    /// The paragraph (lines between blank lines) around `byte`, no final newline.
    pub fn paragraph_at(&self, byte: usize) -> (usize, usize) {
        let text = &self.content;
        let byte = floor_char(text, byte.min(text.len()));
        let start = text[..byte].rfind("\n\n").map_or(0, |i| i + 2);
        let end = text[byte..].find("\n\n").map_or(text.len(), |i| byte + i);
        let end = start.max(text[..end].strip_suffix('\n').map_or(end, str::len));
        (start, end)
    }

    /// Carry `[start, end)` from this layout to `new` by the document's byte
    /// shift, never by searching for the text: unchanged through `end` keeps
    /// the bytes; after a head trim the old suffix aligns with the start of
    /// `new` and the range moves by the trimmed length. `None` once any of it
    /// was trimmed or rewritten.
    pub fn remap_range(&self, new: &Self, (start, end): (usize, usize)) -> Option<(usize, usize)> {
        let (old, new) = (self.content.as_ref(), new.content.as_ref());
        if old.get(start..end).is_none_or(str::is_empty) {
            return None;
        }
        if new.get(..end) == old.get(..end) {
            return Some((start, end));
        }
        // Head trim: the smallest k with old[k..end] == new[..end - k]. The
        // candidates are where new's first line (which survived the trim)
        // sits in old; the probe stays short of any appended tail.
        let lead = new.len() - new.trim_start_matches('\n').len();
        let mut probe = new[lead..]
            .find('\n')
            .map_or(new.len(), |i| lead + i)
            .min(lead + 64);
        while !new.is_char_boundary(probe) {
            probe -= 1;
        }
        if probe == lead {
            return None;
        }
        old.match_indices(&new[..probe])
            .map(|(k, _)| k)
            .filter(|&k| k > 0)
            .take_while(|&k| k <= start)
            .find(|&k| new.get(..end - k) == old.get(k..end))
            .map(|k| (start - k, end - k))
    }

    /// Tile-local copy button for block `index` at `scroll_y`, `None` when its
    /// first row is outside the viewport. Sized from the document line height.
    pub fn copy_button_rect(&self, index: usize, scroll_y: f32) -> Option<Rect> {
        let (start, _) = *self.reading.code_blocks.get(index)?;
        let row = self.rows.iter().rposition(|(b, _)| *b <= start)?;
        let h = self.line_height.max(1.0);
        let w = (h * 4.0).min(self.viewport.width);
        let y = self.rows[row].1 - scroll_y;
        (y >= 0.0 && y + h <= self.viewport.height).then(|| {
            Rect::new(
                self.viewport.x + self.viewport.width - w,
                self.viewport.y + y,
                w,
                h,
            )
        })
    }
}

fn floor_char(text: &str, mut i: usize) -> usize {
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FontFamily, Rgba, SceneId, TextAlign, TextMarkdownNode, TextOverflow};

    /// Monospace fixture: 10 px per byte, 20 px rows, one row per line.
    pub(crate) fn layout(text: &str) -> PortalDocumentLayout {
        let mut rows = Vec::new();
        let mut glyphs = Vec::new();
        let mut row_glyphs = Vec::new();
        let mut start = 0;
        for (i, line) in text.split('\n').enumerate() {
            rows.push((start, i as f32 * 20.0));
            row_glyphs.push(glyphs.len() as u32);
            for (j, c) in line.char_indices() {
                glyphs.push(PortalGlyph {
                    start: (start + j) as u32,
                    end: (start + j + c.len_utf8()) as u32,
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

    #[test]
    fn https_rule_matches_the_card_url_rule() {
        assert_eq!(
            https_link_target("https://example.com/a").as_deref(),
            Some("https://example.com/a")
        );
        for bad in [
            "http://example.com",
            "file:///etc/passwd",
            "https://",
            "mailto:a@b",
            "x",
        ] {
            assert_eq!(https_link_target(bad), None, "{bad}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_LINK_BYTES));
        assert_eq!(https_link_target(&long), None);
    }

    #[test]
    fn byte_and_glyph_hits_follow_rows_and_centres() {
        let l = layout("hello world\nsecond");
        assert_eq!(l.byte_at(0.0, 5.0), 0);
        assert_eq!(l.byte_at(14.0, 5.0), 1, "past the first glyph's centre");
        assert_eq!(
            l.byte_at(500.0, 5.0),
            11,
            "row end stops before the newline"
        );
        assert_eq!(l.byte_at(500.0, 25.0), 18);
        assert_eq!(l.byte_at(0.0, 999.0), 12, "below clamps to the last row");
        assert_eq!(l.glyph_at(5.0, 25.0), Some(12));
        assert_eq!(l.glyph_at(65.0, 25.0), None, "past the text is not text");
    }

    #[test]
    fn word_and_paragraph_boundaries() {
        let l = layout("one two_x, three\nsame para\n\nnext para");
        assert_eq!(l.word_at(5), (4, 9));
        assert_eq!(l.word_at(9), (9, 10), "punctuation selects itself");
        assert_eq!(l.paragraph_at(2), (0, 26));
        assert_eq!(l.paragraph_at(30), (28, 37));
    }

    #[test]
    fn range_rects_cover_each_row_of_a_selection() {
        let l = layout("abcd\nefgh\nijkl");
        let rects = l.range_rects(2, 12);
        assert_eq!(
            rects,
            vec![
                Rect::new(20.0, 0.0, 20.0, 20.0),
                Rect::new(0.0, 20.0, 40.0, 20.0),
                Rect::new(0.0, 40.0, 20.0, 20.0),
            ]
        );
    }

    #[test]
    fn remap_keeps_text_through_appends_and_head_trim_and_drops_evicted() {
        let old = layout("alpha\nbeta\ngamma");
        let sel = (6, 10);
        assert_eq!(
            old.remap_range(&layout("alpha\nbeta\ngamma\ndelta"), sel),
            Some(sel)
        );
        assert_eq!(
            old.remap_range(&layout("beta\ngamma\ndelta"), sel),
            Some((0, 4))
        );
        assert_eq!(old.remap_range(&layout("gamma\ndelta"), sel), None);
    }

    #[test]
    fn remap_follows_the_byte_shift_not_another_occurrence_of_the_text() {
        // R2.2 S3 at the retention cap: every append trims the head, and the
        // selected word repeats on other lines. It must stay on line b.
        let old = layout("a error 1\nb error 2\nc error 3");
        let sel = (12, 17); // "error" on line b
        assert_eq!(&old.content[sel.0..sel.1], "error");
        let trimmed = layout("b error 2\nc error 3\nd error 4");
        assert_eq!(old.remap_range(&trimmed, sel), Some((2, 7)));
        // Rewritten selected text (keyed replacement) drops the selection,
        // even though the same word exists elsewhere.
        let rewritten = layout("a error 1\nb warn 2\nc error 3");
        assert_eq!(old.remap_range(&rewritten, sel), None);
        // A change after the selection keeps the bytes.
        let tail = layout("a error 1\nb error 2\nc error 9");
        assert_eq!(old.remap_range(&tail, sel), Some(sel));
    }

    #[test]
    fn copy_button_sits_at_the_block_top_right_only_when_visible() {
        let mut l = layout("intro\ncode\nmore");
        l.reading.code_blocks = vec![(6, 10)].into();
        assert_eq!(
            l.copy_button_rect(0, 0.0),
            Some(Rect::new(320.0, 20.0, 80.0, 20.0))
        );
        assert_eq!(l.copy_button_rect(0, 30.0), None, "scrolled above the view");
        assert_eq!(l.copy_button_rect(1, 0.0), None);
    }

    #[test]
    fn link_host_is_the_href_host() {
        let link = PortalLink {
            start: 0,
            end: 4,
            href: "https://docs.example.com/x?q=1".into(),
        };
        assert_eq!(link.host().as_deref(), Some("docs.example.com"));
    }
}
