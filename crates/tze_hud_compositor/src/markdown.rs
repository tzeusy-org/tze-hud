//! Phase-1 Markdown subset parser for tze_hud portal text.
//!
//! # Design contract
//!
//! Parsing happens **at content-commit time**, never on the per-frame render
//! path.  The compositor calls [`MarkdownCache::prime`] once after each
//! scene commit, which parses any new or changed [`TextMarkdownNode`] content
//! and stores the result in a BLAKE3-keyed cache.  The frame pipeline
//! consumes only cached [`ParsedMarkdown`] values — a node whose content has
//! not changed incurs **zero** parse cost per frame.
//!
//! # Subset
//!
//! Supported constructs (rendered with token-driven styling):
//! - ATX headings (H1–H6)
//! - Bold (`**text**` / `__text__`)
//! - Italic (`*text*` / `_text_`)
//! - Bold-italic (`***text***`)
//! - Inline code (`` `code` ``)
//! - Fenced code blocks (`` ``` `` and `~~~`)
//! - Indented code blocks (4-space / tab indent)
//! - Ordered lists (` 1. item`)
//! - Unordered lists (`- item`, `* item`, `+ item`)
//! - Links: `[text](https://…)`, `<https://…>` and bare `https://…` keep their
//!   validated HTTPS target (hud-ljhku.2); any other scheme renders as plain text
//!
//! Excluded constructs (rendered as literal source text, never silently
//! dropped):
//! - Tables
//! - Images (`![alt](url)`)
//! - Raw HTML
//! - Blockquotes (`> text`)
//! - Footnotes
//! - Strikethrough (`~~text~~`)
//! - Task lists (`- [ ] item`)
//! - Non-HTTPS autolinks (`<http://...>`)
//!
//! # Styling
//!
//! All visual values come from [`MarkdownTokens`], which is resolved from the
//! runtime's design-token map at startup (and on any token-map update).  No
//! values are hardcoded in this module; callers that have not yet loaded a
//! token map receive sensible defaults.
//!
//! [`TextMarkdownNode`]: tze_hud_scene::types::TextMarkdownNode
//! [`MarkdownCache::prime`]: MarkdownCache::prime

use std::collections::HashMap;
use std::sync::Arc;

use tze_hud_scene::types::Rgba;

// ─── MarkdownTokens ──────────────────────────────────────────────────────────

/// Token-resolved styling values for the Phase-1 markdown subset.
///
/// All fields carry defaults so rendering works before a token map is loaded.
/// Populated by [`MarkdownTokens::from_token_map`].
///
/// # Token keys
///
/// | Key | Effect |
/// |-----|--------|
/// | `typography.code.family` | `"monospace"` enables monospace for code spans |
/// | `color.code.background` | Backdrop panel color drawn behind inline/fenced code |
/// | `color.code.text` | Optional foreground color for code spans and blocks |
/// | `color.link.text` | Link text color |
/// | `typography.bold.weight` | CSS weight for bold (`**text**`) spans; default 700 |
/// | `typography.heading.N.weight` | CSS weight for heading level N (1–6) |
/// | `typography.heading.N.scale` | Font-size multiplier for heading level N |
/// | `typography.emphasis.italic` | Whether emphasis uses italic (always true) |
/// | `typography.line_height.multiplier` | Line-height = font_size_px × this value; default 1.4 |
/// | `spacing.heading.top` | Blank-line multiplier above headings; default 0.0 |
/// | `spacing.heading.bottom` | Blank-line multiplier below headings; default 0.0 |
/// | `spacing.list.item` | Blank-line multiplier between list items; default 0.0 (tight) |
#[derive(Clone, Debug)]
pub struct MarkdownTokens {
    /// Font size multiplier per heading level (index 0 = H1, 5 = H6).
    pub heading_scale: [f32; 6],
    /// CSS font weight per heading level (index 0 = H1, 5 = H6).
    pub heading_weight: [u16; 6],
    /// CSS font weight for bold (`**text**`) spans.  Default 700.
    ///
    /// Resolved from `typography.bold.weight`.  Bold-italic inherits the same weight.
    pub bold_weight: u16,
    /// Color override for link text.  `None` = no override (falls back to node color).
    pub link_color: Option<Rgba>,
    /// Whether inline code and code blocks use the monospace family.  Defaults to `true`.
    pub code_monospace: bool,
    /// Optional foreground color for code spans and blocks.  `None` = no override.
    ///
    /// Resolved from `color.code.text`.
    pub code_color: Option<Rgba>,
    /// Optional backdrop panel color drawn behind inline code and fenced/indented
    /// code blocks.
    ///
    /// Resolved from `color.code.background`.  Rendered as a geometry quad behind the
    /// code text — does **not** affect the text foreground color.  Use `code_color`
    /// (resolved from `color.code.text`) to change the code text color.
    /// `None` = no backdrop panel.
    pub code_background: Option<Rgba>,
    /// Line-height multiplier: `line_height_px = font_size_px × line_height_multiplier`.
    ///
    /// Resolved from `typography.line_height.multiplier`.  Default 1.4.
    /// Must be in `[1.0, 4.0]`; values outside that range are ignored.
    pub line_height_multiplier: f32,
    /// Blank-line multiplier applied **above** each heading block.
    ///
    /// `spacing.heading.top`: a value of `1.0` inserts one blank line; `0.0`
    /// suppresses spacing.  Default `0.0`.  Must be in `[0.0, 4.0]`.
    pub heading_margin_top: f32,
    /// Blank-line multiplier applied **below** each heading block.
    ///
    /// `spacing.heading.bottom`: a value of `0.5` inserts a half-blank line;
    /// values ≥ `1.0` insert a full blank line.  Default `0.0` keeps streamed
    /// transcript blocks tight.  Must be in `[0.0, 4.0]`.
    pub heading_margin_bottom: f32,
    /// Blank-line multiplier between consecutive list items.
    ///
    /// `spacing.list.item`: `0.0` (default) renders tight lists (no extra
    /// blank line between items); `1.0` inserts one blank line between items.
    /// Must be in `[0.0, 4.0]`.
    pub list_item_spacing: f32,
    /// Color of a thematic-break (`---`/`***`/`___`) divider — the transcript
    /// turn separator. `None` disables separator rendering: a
    /// thematic-break line then renders as ordinary text (no divider), preserving
    /// pre-existing behavior for surfaces with no separator token configured.
    ///
    /// Resolved from `portal.divider.color`.  Rendered as a thin geometry quad on
    /// the break's line — content-free, so it carries no text and reveals nothing
    /// under redaction (the transcript units are zeroed upstream when redacted).
    pub separator_color: Option<Rgba>,
    /// Thickness (physical px) of the thematic-break divider quad.
    ///
    /// Resolved from `portal.divider.thickness_px`.  Default `1.0`; clamped to a
    /// sane minimum at render time so a stray `0` cannot make the divider vanish.
    pub separator_thickness_px: f32,
}

impl Default for MarkdownTokens {
    fn default() -> Self {
        // Sensible defaults: heading weight decreases with level, scale reflects
        // a modest typographic ramp.  These match the canonical token schema
        // described in the spec (no token key = fall back to these values).
        Self {
            heading_scale: [1.75, 1.50, 1.25, 1.10, 1.00, 0.90],
            heading_weight: [700, 700, 700, 700, 600, 600],
            bold_weight: 700,
            link_color: None,
            code_monospace: true,
            code_color: None,
            code_background: None,
            line_height_multiplier: crate::text::LINE_HEIGHT_MULTIPLIER,
            heading_margin_top: 0.0,
            heading_margin_bottom: 0.0,
            list_item_spacing: 0.0,
            separator_color: None,
            separator_thickness_px: 1.0,
        }
    }
}

/// Which markdown surface a token set is being resolved for.
///
/// The text-stream portal transcript is a *governed* markdown surface: it may
/// restyle its own code/link/divider treatment via `portal.*`-namespaced token
/// keys. Any *other* markdown surface must resolve only
/// the generic (`color.*` / `typography.*`) keys, so a portal-scoped preference
/// can never leak onto it.
///
/// In v1 the portal transcript is the sole governed markdown surface (zones use
/// `StreamText`, not markdown), so every markdown node resolves [`Portal`]. This
/// enum exists so that the moment a non-portal markdown surface is introduced,
/// its tokens resolve [`Generic`] and the parse cache — keyed on the token-set
/// identity (see [`MarkdownTokens::hash_into`]) — keeps the two scopes' parses
/// from colliding on identical content.
///
/// [`Portal`]: MarkdownScope::Portal
/// [`Generic`]: MarkdownScope::Generic
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MarkdownScope {
    /// The governed text-stream portal transcript surface: prefer `portal.*`
    /// namespaced keys over their generic fallbacks.
    Portal,
    /// Any non-portal markdown surface: resolve only the generic keys; ignore
    /// every `portal.*`-namespaced token.
    Generic,
}

impl MarkdownTokens {
    /// Resolve styling from a design-token map for the **portal** surface.
    ///
    /// Equivalent to [`MarkdownTokens::from_token_map_scoped`] with
    /// [`MarkdownScope::Portal`]. This is the v1 behavior (the portal transcript
    /// is the sole governed markdown surface); it is kept as the ergonomic
    /// default so existing call sites are unchanged.
    ///
    /// Unrecognised or unparseable token values fall back to [`Default`]
    /// values, so callers receive sensible rendering even with a partial or
    /// empty token map.
    pub fn from_token_map(map: &HashMap<String, String>) -> Self {
        Self::from_token_map_scoped(map, MarkdownScope::Portal)
    }

    /// Resolve styling from a design-token map for a specific markdown
    /// [`MarkdownScope`].
    ///
    /// [`MarkdownScope::Portal`] prefers `portal.*`-namespaced keys (code/link
    /// treatment, divider) over their generic fallbacks;
    /// [`MarkdownScope::Generic`] ignores every `portal.*` key and resolves only
    /// the generic (`color.*` / `typography.*` / `spacing.*`) keys, so a
    /// portal-scoped preference can never reach a non-portal surface.
    pub(crate) fn from_token_map_scoped(
        map: &HashMap<String, String>,
        scope: MarkdownScope,
    ) -> Self {
        let mut tokens = Self::default();

        // Heading weights: typography.heading.{1..6}.weight
        for (i, level) in (1u8..=6).enumerate() {
            let key = format!("typography.heading.{level}.weight");
            if let Some(w) = map.get(&key).and_then(|v| v.parse::<u16>().ok()) {
                if (100..=900).contains(&w) {
                    tokens.heading_weight[i] = w;
                }
            }
        }

        // Heading scales: typography.heading.{1..6}.scale
        for (i, level) in (1u8..=6).enumerate() {
            let key = format!("typography.heading.{level}.scale");
            if let Some(s) = map.get(&key).and_then(|v| v.parse::<f32>().ok()) {
                if s > 0.0 {
                    tokens.heading_scale[i] = s;
                }
            }
        }

        // Portal markdown-subset preference (Promotion P2): for the
        // markdown-subset code/link styling the transcript renders, prefer the
        // portal-scoped canonical keys (`portal.transcript.*`) over the generic
        // ones (`color.code.*` / `color.link.text` / `typography.code.family`),
        // falling back to the generic key when the portal key is unset. This lets
        // a portal profile restyle its own code/link treatment while the generic
        // markdown defaults are preserved when no portal key is present.
        //
        // Per-tile scoping: for the portal surface, prefer the
        // portal-scoped key over the generic fallback; for any non-portal
        // surface (`Generic`), resolve ONLY the generic key so a portal
        // preference can never leak onto it. The resolved token-set identity is
        // folded into the parse cache key (see `hash_into`), so the two scopes'
        // parses of the same content never collide.
        let prefer = |portal_key: &str, generic_key: &str| match scope {
            MarkdownScope::Portal => map.get(portal_key).or_else(|| map.get(generic_key)),
            MarkdownScope::Generic => map.get(generic_key),
        };

        // Link color: portal.transcript.link_color → color.link.text (hex).
        if let Some(c) = prefer("portal.transcript.link_color", "color.link.text")
            .and_then(|v| parse_hex_color(v))
        {
            tokens.link_color = Some(c);
        }

        // Code family: portal.transcript.code_font_family → typography.code.family
        // = "monospace" | "sans-serif" |...
        if let Some(fam) = prefer(
            "portal.transcript.code_font_family",
            "typography.code.family",
        ) {
            tokens.code_monospace = fam.to_lowercase().contains("mono");
        }

        // Code foreground: portal.transcript.code_text → color.code.text.
        if let Some(c) = prefer("portal.transcript.code_text", "color.code.text")
            .and_then(|v| parse_hex_color(v))
        {
            tokens.code_color = Some(c);
        }

        // Code background: portal.transcript.code_background → color.code.background.
        // Phase 1: stored for use as a foreground color modifier when no
        // code_color is set (see StyleAttr::code_effective_color).
        if let Some(c) = prefer("portal.transcript.code_background", "color.code.background")
            .and_then(|v| parse_hex_color(v))
        {
            tokens.code_background = Some(c);
        }

        // Bold weight: typography.bold.weight
        if let Some(w) = map
            .get("typography.bold.weight")
            .and_then(|v| v.parse::<u16>().ok())
        {
            if (100..=900).contains(&w) {
                tokens.bold_weight = w;
            }
        }

        // Line-height multiplier: typography.line_height.multiplier
        if let Some(m) = map
            .get("typography.line_height.multiplier")
            .and_then(|v| v.parse::<f32>().ok())
        {
            if m.is_finite() && (1.0..=4.0).contains(&m) {
                tokens.line_height_multiplier = m;
            }
        }

        // Heading block spacing: spacing.heading.top / spacing.heading.bottom
        if let Some(v) = map
            .get("spacing.heading.top")
            .and_then(|v| v.parse::<f32>().ok())
        {
            if v.is_finite() && (0.0..=4.0).contains(&v) {
                tokens.heading_margin_top = v;
            }
        }
        if let Some(v) = map
            .get("spacing.heading.bottom")
            .and_then(|v| v.parse::<f32>().ok())
        {
            if v.is_finite() && (0.0..=4.0).contains(&v) {
                tokens.heading_margin_bottom = v;
            }
        }

        // List item spacing: spacing.list.item
        if let Some(v) = map
            .get("spacing.list.item")
            .and_then(|v| v.parse::<f32>().ok())
        {
            if v.is_finite() && (0.0..=4.0).contains(&v) {
                tokens.list_item_spacing = v;
            }
        }

        // Transcript turn separator: portal.divider.color /.thickness_px.
        // These are `portal.`-namespaced, so they resolve for the portal scope
        // only; a non-portal (`Generic`) surface leaves the separator disabled
        // (a `---` line renders as ordinary text).
        if scope == MarkdownScope::Portal {
            if let Some(c) = map
                .get("portal.divider.color")
                .and_then(|v| parse_hex_color(v))
            {
                tokens.separator_color = Some(c);
            }
            if let Some(t) = map
                .get("portal.divider.thickness_px")
                .and_then(|v| v.parse::<f32>().ok())
            {
                if t.is_finite() && t > 0.0 {
                    tokens.separator_thickness_px = t;
                }
            }
            // A hairline: dp × px per dp, whole pixels, never thinner than one.
            tokens.separator_thickness_px = (tokens.separator_thickness_px
                * tze_hud_config::tokens::px_scale(map))
            .round()
            .max(1.0);
        }

        tokens
    }

    /// Fold every parse-affecting styling field into `hasher` so the markdown
    /// parse cache can key on the *token-set identity* alongside the content.
    ///
    /// Two `MarkdownTokens` with identical field values hash the same and are
    /// interchangeable — they produce byte-identical [`ParsedMarkdown`], so
    /// sharing a cache entry is correct. Two token sets that differ in any
    /// parse-affecting field (e.g. a portal scope that resolved
    /// `portal.transcript.link_color` vs. a generic scope that did not) hash
    /// differently, so the same content under each scope occupies distinct cache
    /// entries and can never be mis-served across scopes.
    ///
    /// Every field of [`MarkdownTokens`] influences [`parse_markdown_subset`]'s
    /// output (span styling, code panels, thematic breaks, line metrics), so all
    /// are folded in. `f32` fields are hashed by their raw little-endian bits;
    /// styling never uses `NaN`, so bit-hashing is a stable identity.
    fn hash_into(&self, hasher: &mut blake3::Hasher) {
        for s in self.heading_scale {
            hasher.update(&s.to_le_bytes());
        }
        for w in self.heading_weight {
            hasher.update(&w.to_le_bytes());
        }
        hasher.update(&self.bold_weight.to_le_bytes());
        hash_opt_rgba(hasher, self.link_color);
        hasher.update(&[self.code_monospace as u8]);
        hash_opt_rgba(hasher, self.code_color);
        hash_opt_rgba(hasher, self.code_background);
        hasher.update(&self.line_height_multiplier.to_le_bytes());
        hasher.update(&self.heading_margin_top.to_le_bytes());
        hasher.update(&self.heading_margin_bottom.to_le_bytes());
        hasher.update(&self.list_item_spacing.to_le_bytes());
        hash_opt_rgba(hasher, self.separator_color);
        hasher.update(&self.separator_thickness_px.to_le_bytes());
    }
}

/// Fold an `Option<Rgba>` into `hasher` with a presence tag so `None` and a
/// fully-transparent color hash differently.
fn hash_opt_rgba(hasher: &mut blake3::Hasher, color: Option<Rgba>) {
    match color {
        None => {
            hasher.update(&[0u8]);
        }
        Some(c) => {
            hasher.update(&[1u8]);
            hasher.update(&c.r.to_le_bytes());
            hasher.update(&c.g.to_le_bytes());
            hasher.update(&c.b.to_le_bytes());
            hasher.update(&c.a.to_le_bytes());
        }
    }
}

// ─── StyleAttr ───────────────────────────────────────────────────────────────

/// Style attributes that can be applied to a byte range in the plain-text output.
///
/// These are compositor-native: they are used to build glyphon `Attrs` spans
/// in `text.rs` without any per-frame string manipulation.
#[derive(Clone, Debug, PartialEq)]
pub struct StyleAttr {
    /// Overrides the base font weight if `Some`.  E.g. `Some(700)` for bold.
    pub weight: Option<u16>,
    /// When `true`, the italic style variant is requested.
    pub italic: bool,
    /// When `true`, monospace family is used instead of the node default.
    pub monospace: bool,
    /// Overrides the text color if `Some`.
    pub color: Option<Rgba>,
    /// Backdrop panel color drawn behind this span's glyph area if `Some`.
    ///
    /// Populated for code spans (inline/fenced/indented) when the
    /// `color.code.background` design token is set.  Rendered as a geometry
    /// quad behind the text — does **not** affect the text foreground color.
    pub background_color: Option<Rgba>,
    /// Font-size multiplier relative to the node's base `font_size_px`.
    ///
    /// `None` means no scaling (uses the node base size).  `Some(1.75)`
    /// renders the span at 175% of the node's base font size.  Used to
    /// apply heading-level scale from `MarkdownTokens::heading_scale`.
    pub size_scale: Option<f32>,
    /// Validated HTTPS link target when this span is link text (hud-ljhku.2).
    pub href: Option<Arc<str>>,
}

impl StyleAttr {
    /// The "no styling" identity — used for spans with no markdown decoration.
    pub(crate) fn plain() -> Self {
        Self {
            weight: None,
            italic: false,
            monospace: false,
            color: None,
            background_color: None,
            size_scale: None,
            href: None,
        }
    }
}

// ─── StyledSpan ──────────────────────────────────────────────────────────────

/// A styled span in the [`ParsedMarkdown::plain_text`] output.
///
/// Byte offsets are valid UTF-8 boundaries into [`ParsedMarkdown::plain_text`].
/// The span is exclusive-end: `[start_byte, end_byte)`.
#[derive(Clone, Debug, PartialEq)]
pub struct StyledSpan {
    /// Inclusive byte offset into `ParsedMarkdown::plain_text`.
    pub start_byte: usize,
    /// Exclusive byte offset into `ParsedMarkdown::plain_text`.
    pub end_byte: usize,
    /// Style attributes for this span.
    pub attr: StyleAttr,
}

// ─── CodePanelSpan ───────────────────────────────────────────────────────────

/// The kind of code region for backdrop-panel rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodePanelKind {
    /// Fenced code block (`` ``` `` or `~~~`) or indented code block (4-space/tab).
    Block,
    /// Inline code span (`` `code` ``).
    Inline,
}

/// A contiguous region in [`ParsedMarkdown::plain_text`] that should receive a
/// code-panel backdrop quad in the geometry pass.
///
/// Byte offsets are valid UTF-8 boundaries into [`ParsedMarkdown::plain_text`].
/// The range is exclusive-end: `[start_byte, end_byte)`.
///
/// The renderer uses these records together with the node's `font_size_px` and
/// `bounds` to approximate per-block backdrop rectangles without needing
/// per-glyph layout information.
#[derive(Clone, Debug, PartialEq)]
pub struct CodePanelSpan {
    /// Inclusive byte offset into `ParsedMarkdown::plain_text`.
    pub start_byte: usize,
    /// Exclusive byte offset into `ParsedMarkdown::plain_text`.
    pub end_byte: usize,
    /// Whether this is a block-level or inline code region.
    pub kind: CodePanelKind,
}

// ─── ListItemSpan ─────────────────────────────────────────────────────────────

/// Metadata for a single list item in [`ParsedMarkdown`].
///
/// Records the byte offset of the item's text content start (after the bullet
/// or ordinal prefix) within [`ParsedMarkdown::plain_text`].  The renderer uses
/// the difference between `content_start_byte` and `item_start_byte` to derive
/// the bullet width, enabling a proper **hanging indent**: continuation lines of
/// a wrapped item align under the text content rather than the bullet marker.
///
/// The indent depth (nesting level) is also recorded so callers can apply a
/// consistent left margin per nesting level without re-scanning the text.
#[derive(Clone, Debug, PartialEq)]
pub struct ListItemSpan {
    /// Byte offset of the first character of the bullet/ordinal prefix (the
    /// start of the full "  • text" string in `plain_text`).
    pub item_start_byte: usize,
    /// Byte offset of the first character of the item text content, just after
    /// the bullet/ordinal and its trailing space (e.g. "• " or "1. ").
    ///
    /// `content_start_byte - item_start_byte` gives the bullet prefix byte
    /// width; used as the hanging-indent offset for continuation lines.
    pub content_start_byte: usize,
    /// Nesting depth (0 = top-level, 1 = first indent, …).
    pub indent_level: u8,
}

// ─── LinkSpan ────────────────────────────────────────────────────────────────

/// A link in [`ParsedMarkdown::plain_text`] with its validated HTTPS target.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkSpan {
    pub start_byte: usize,
    pub end_byte: usize,
    pub href: Arc<str>,
}

// ─── ParsedMarkdown ──────────────────────────────────────────────────────────

/// The cached result of parsing a single `TextMarkdownNode` content string.
///
/// Produced by [`parse_markdown_subset`] and stored by [`MarkdownCache`].
///
/// - `plain_text` is the rendered text (markup stripped; excluded constructs
///   preserve literal source text).
/// - `spans` covers only runs with non-plain styling; unstyled gaps between
///   spans inherit the node's base style.
/// - `code_panels` lists byte ranges that should receive a backdrop panel quad
///   (populated when the `color.code.background` token is set).
/// - `list_items` records per-item bullet metadata for hanging-indent layout.
/// - `line_height_multiplier` mirrors the token value for the render path.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedMarkdown {
    /// Plain text output, suitable for glyph layout.
    ///
    /// Stored as `Arc<str>` so the per-frame path in
    /// `TextItem::from_text_markdown_cached` can clone it with a cheap
    /// reference-count bump instead of a deep string copy.
    pub plain_text: Arc<str>,
    /// Styled spans, sorted by `start_byte`, non-overlapping.
    pub spans: Vec<StyledSpan>,
    /// Code-panel backdrop regions, in `plain_text` byte space.
    ///
    /// Non-empty only when `tokens.code_background` is `Some`.  The renderer
    /// iterates these to emit geometry quads behind code text.  Sorted by
    /// `start_byte`.
    pub code_panels: Vec<CodePanelSpan>,
    /// Per-list-item hanging-indent metadata, sorted by `item_start_byte`.
    ///
    /// Empty when the document contains no list items.  The renderer uses
    /// `content_start_byte - item_start_byte` to compute the bullet prefix
    /// width for hanging-indent layout.
    pub list_items: Vec<ListItemSpan>,
    /// Byte offsets (into `plain_text`) of thematic-break divider lines — the
    /// transcript turn separators. Each offset points at the start
    /// of a blank line the renderer draws a token-styled divider quad on.
    ///
    /// Non-empty only when `tokens.separator_color` is `Some`; a thematic break
    /// (`---`/`***`/`___`) is otherwise treated as ordinary text.  Sorted
    /// ascending (parse order is source order).
    pub thematic_breaks: Vec<usize>,
    /// HTTPS links in `plain_text` byte space, sorted (hud-ljhku.2).
    pub links: Vec<LinkSpan>,
    /// Fenced code block contents in `plain_text` byte space, sorted, without
    /// the final newline; the copy button copies exactly this text.
    pub code_blocks: Vec<(usize, usize)>,
    /// Line-height multiplier resolved from the `typography.line_height.multiplier`
    /// token at parse time.
    ///
    /// Passed through to the render path so `text.rs` can compute
    /// `line_height_px = font_size_px × line_height_multiplier` without
    /// needing a separate token-map lookup per frame.
    pub line_height_multiplier: f32,
}

// ─── MarkdownCache ────────────────────────────────────────────────────────────

/// Content-addressed cache of [`ParsedMarkdown`] values.
///
/// The key is a BLAKE3 hash of the raw content bytes.  This gives O(1)
/// lookup: the compositor builds the key from the node's content once
/// per commit, then consults the cache on every frame with zero parse work.
///
/// The cache is unbounded in Phase-1 (content strings are capped at 65535
/// bytes each; total cache growth is bounded by the total number of distinct
/// `TextMarkdownNode` content values alive in the scene).  A proper LRU
/// eviction policy is deferred to a post-promotion follow-up.
#[derive(Default)]
pub struct MarkdownCache {
    /// Keyed by BLAKE3 hash of the raw content string.
    entries: HashMap<[u8; 32], ParsedMarkdown>,
}

impl MarkdownCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached parsed result for `content` under `tokens`, or `None`
    /// if it has not been primed yet.
    ///
    /// The lookup key folds both the content and the token-set identity
    ///, so the same content parsed under a different token set is a
    /// distinct entry — never mis-served.  This hashes `content` on every call
    /// (O(content_bytes)).  Prefer [`MarkdownCache::get_by_key`] when a
    /// precomputed key is available (e.g. stored on the scene node at commit
    /// time) to keep lookups O(1).
    pub fn get(&self, content: &str, tokens: &MarkdownTokens) -> Option<&ParsedMarkdown> {
        let key = Self::compute_key(content, tokens);
        self.entries.get(&key)
    }

    /// Return the cached parsed result for a precomputed BLAKE3 key, or
    /// `None` if it has not been primed yet.
    ///
    /// This is a true O(32-byte) lookup with zero hashing cost.  Use this
    /// on the frame path: compute the key once at content-commit time, store
    /// it on the scene node, and pass it here every frame.
    #[inline]
    pub fn get_by_key(&self, key: &[u8; 32]) -> Option<&ParsedMarkdown> {
        self.entries.get(key)
    }

    /// Compute the BLAKE3 cache key for `content` parsed under `tokens`.
    ///
    /// The key folds the raw content **and** the token-set identity
    /// ([`MarkdownTokens::hash_into`]) behind a domain separator, so the same
    /// content under two different token sets (e.g. a portal vs. a non-portal
    /// surface) yields two distinct keys and never collides in the cache.
    /// Two token sets with identical parse-affecting fields hash
    /// the same — correct, since they produce identical parses.
    ///
    /// Call this once at content-commit time with the tile's scoped tokens and
    /// store the key alongside the node so frame-time lookups can use
    /// [`MarkdownCache::get_by_key`].
    #[inline]
    pub fn compute_key(content: &str, tokens: &MarkdownTokens) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(content.as_bytes());
        // Domain separator so a token identity can never be confused with a
        // trailing content byte sequence.
        hasher.update(b"\x1fmarkdown-token-set\x1f");
        tokens.hash_into(&mut hasher);
        *hasher.finalize().as_bytes()
    }

    /// Parse and cache the content if it is not already present.
    ///
    /// Returns a reference to the cached result.  Calling this for the same
    /// content string twice is a no-op after the first call.
    ///
    /// This method is called at content-commit time, **not** on the frame path.
    pub fn prime<'a>(&'a mut self, content: &str, tokens: &MarkdownTokens) -> &'a ParsedMarkdown {
        let key = Self::compute_key(content, tokens);
        // Use entry API to avoid double-hashing.
        self.entries
            .entry(key)
            .or_insert_with(|| parse_markdown_subset(content, tokens))
    }

    /// Number of distinct content hashes currently cached.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the cache is empty.
    #[inline]
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Evict all entries whose raw content is no longer in `live_keys`.
    ///
    /// Each element of `live_keys` should be the BLAKE3 hash of a content
    /// string still referenced by the scene.  This keeps the cache bounded
    /// when scene nodes are removed.
    #[cfg(test)]
    pub(crate) fn evict_except(&mut self, live_keys: &[[u8; 32]]) {
        let keep: std::collections::HashSet<[u8; 32]> = live_keys.iter().copied().collect();
        self.entries.retain(|k, _| keep.contains(k));
    }

    /// Build the next complete cache snapshot for a live node set, reusing
    /// already-parsed entries from `base` and parsing only the content that is
    /// missing.
    ///
    /// This is the pure, side-effect-free core of the async prime
    /// ([`MarkdownPrimer`]): it can run on any thread (the result is plain data
    /// behind [`Arc`]) and the produced snapshot is **complete** — it contains
    /// exactly the entries for `jobs` (carried forward from `base` when already
    /// parsed, freshly parsed otherwise) and nothing else.  Dropping entries
    /// not present in `jobs` performs eviction implicitly: the returned snapshot
    /// is the live set, so swapping it in atomically also prunes dead content.
    ///
    /// Because every produced snapshot is internally consistent, a reader that
    /// races the swap sees either `base` or the new snapshot in full — never a
    /// torn state.
    fn rebuild_from(base: &MarkdownCache, jobs: &[PrimeJob]) -> Self {
        let mut entries: HashMap<[u8; 32], ParsedMarkdown> = HashMap::with_capacity(jobs.len());
        for job in jobs {
            if entries.contains_key(&job.key) {
                // Duplicate content within the live set — already inserted.
                continue;
            }
            if let Some(existing) = base.entries.get(&job.key) {
                // Carry forward the already-parsed entry (cheap Arc/Vec clone).
                entries.insert(job.key, existing.clone());
            } else if let Some(content) = &job.content {
                // Parse with the job's own scoped tokens: each job
                // carries the token set its owning tile resolved (portal vs.
                // generic), so a single rebuild may mix scopes.  `job.key`
                // already folds this token identity, so entries never collide.
                entries.insert(job.key, parse_markdown_subset(content, &job.tokens));
            }
            // A job with no cached entry and `content: None` cannot be parsed;
            // it is simply omitted, and the render path's cache-miss fallback
            // parses it inline.  This only occurs if a caller mislabels a job.
        }
        Self { entries }
    }
}

// ─── MarkdownPrimer ───────────────────────────────────────────────────────────

/// A single live `TextMarkdownNode` in the prime set, identified by its
/// precomputed BLAKE3 content key.
///
/// `content` is `Some` only when the caller could not find `key` in the current
/// snapshot (i.e. it must be parsed).  Already-cached entries pass `None`: the
/// rebuild carries them forward from the previous snapshot by key, so their
/// (up to 64 KiB) source string is never re-cloned on the commit path.  When
/// present, the source is held as `Arc<str>` so the job ships to the background
/// parse thread without copying.
#[derive(Clone)]
pub(crate) struct PrimeJob {
    /// BLAKE3 cache key (see [`MarkdownCache::compute_key`]) — folds the content
    /// **and** [`Self::tokens`]' identity, so a portal and a non-portal surface
    /// with identical content occupy distinct entries.
    pub(crate) key: [u8; 32],
    /// Raw markdown source — `Some` only when this key needs parsing.
    pub(crate) content: Option<Arc<str>>,
    /// The scoped token set this content must be parsed with.
    ///
    /// Selected per owning tile: [`MarkdownScope::Portal`] tokens for a governed
    /// portal transcript surface, [`MarkdownScope::Generic`] tokens otherwise.
    /// Held behind [`Arc`] so the two shared scope token sets are cloned by a
    /// refcount bump rather than a deep copy, and ship to the background parse
    /// thread without duplication.  `key` must have been computed with this same
    /// token set, or a cache miss will result.
    pub(crate) tokens: Arc<MarkdownTokens>,
}

/// Total source bytes below which a prime is parsed inline on the calling
/// (commit) thread rather than dispatched to the background parse thread.
///
/// Small payloads parse in well under the commit-stage budget, so the channel
/// hop + thread wakeup would cost more than the parse itself.  Large payloads —
/// the case this design exists for — clear the threshold and parse off-thread.
/// 4 KiB is roughly the point where parse time begins to matter against the
/// Stage 4 budget on the reference hardware (the `markdown_cache` bench shows a
/// 64 KiB payload dominating; a few KiB is negligible).
const INLINE_PARSE_BYTE_THRESHOLD: usize = 4096;

/// Async, lock-free front end for the markdown parse cache.
///
/// # What this adds over [`MarkdownCache`]
///
/// (Option A) moved parsing **off the per-frame render path** by
/// priming at scene-commit time — but the parse still ran synchronously on the
/// commit thread.  For very large payloads that parse cost lands on the commit
/// thread and can eat into the Stage 4 budget.
///
/// `MarkdownPrimer` completes the move: it owns an [`Arc<ArcSwap<MarkdownCache>>`]
/// that readers `load()` lock-free on the render path, and a dedicated
/// background OS thread that parses large payloads and `store()`s the freshly
/// built snapshot atomically.  Small payloads are parsed inline (see
/// [`INLINE_PARSE_BYTE_THRESHOLD`]) to avoid the channel/wakeup overhead.
///
/// # Why a dedicated thread, not rayon/tokio
///
/// The compositor crate does not depend on rayon or a tokio runtime (only a
/// dev-dependency), and the dependency policy prefers std
/// over new crates.  A single owned `std::thread` mirrors how the runtime crate
/// already structures off-thread work (`threads.rs`, `chrome.rs`) and adds no
/// new dependency.  One worker thread suffices: prime jobs are serialized per
/// scene commit, so there is no fan-out parallelism to exploit.
///
/// # Correctness
///
/// - **No torn reads.** [`MarkdownCache::rebuild_from`] always produces a
///   *complete* snapshot; the reader `load()`s an [`Arc`] to one whole snapshot.
///   It sees either the old or the new cache, never a half-populated map.
/// - **No stale clobber.** Each store is guarded by a monotonically increasing
///   internal epoch ([`Self::published_epoch`]).  A late background result for
///   an older dispatch is dropped rather than overwriting a newer snapshot.
/// - **Liveness fallback.** If the background parse has not completed when a
///   frame renders, the render path's existing cache-miss branch parses that one
///   node inline (non-lossy), so a racing frame is always correct — just not
///   zero-cost for that single frame until the swap lands.
///
/// # Why the ordering token is independent of `SceneGraph::version`
///
/// Earlier versions of this type gated the stale-clobber guard on the
/// caller-supplied `scene.version` directly. That is broken: `SceneGraph::version`
/// is only guaranteed monotonically increasing across the lifetime of ONE
/// `SceneGraph` instance — it restarts at 0 for every newly constructed
/// `SceneGraph`. A `Compositor` (and therefore its one long-lived
/// `MarkdownPrimer`) that gets reused across multiple, independently
/// constructed `SceneGraph`s — e.g. a benchmark/test loop that builds a fresh
/// scene per scenario iteration while reusing one `Compositor`, such as
/// `scroll_reshape_reuse` — can dispatch a legitimate prime for a NEW
/// scene whose `version` is *lower* than the value already recorded from a
/// PRIOR scene. Gating on that raw number then makes `publish_if_newer` treat
/// the new scene's correctly-parsed background result as "stale" and silently,
/// PERMANENTLY drop it: the cache never gains an entry for that content's key,
/// so every subsequent frame's `collect_text_items_from_node` lookup misses and
/// pays a full synchronous re-parse forever (the documented-as-rare inline
/// fallback in `renderer/text.rs`). For example, a new scene dispatching
/// `version=4` while `published_version` holds `5` from the prior scene has its
/// valid result discarded because `4 < 5`.
///
/// The fix: `MarkdownPrimer` now stamps every [`Self::prime`]/[`Self::reset`]
/// dispatch with its OWN internally generated, ever-increasing epoch
/// ([`Self::next_epoch`]) instead of trusting the caller's scene-relative
/// number. An epoch depends only on call order on this primer, so it can never
/// go backwards regardless of how many different `SceneGraph` instances share
/// it over its lifetime.
pub(crate) struct MarkdownPrimer {
    /// The current cache snapshot, swapped atomically.  Readers `load()` it
    /// lock-free; the commit thread and the background worker `store()` it.
    cache: Arc<arc_swap::ArcSwap<MarkdownCache>>,
    /// Highest internal epoch for which a snapshot has been published. Guards
    /// against a late background result clobbering a newer snapshot. See the
    /// note on [`MarkdownPrimer`] for why this is an
    /// internally-generated epoch rather than the caller's scene version.
    published_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Source of the next epoch handed out by [`Self::prime`]/[`Self::reset`].
    /// Bumped once per dispatch, before the work is queued/performed, so
    /// ordering reflects dispatch order (not completion order) — the same
    /// property the old scene-version-based scheme relied on, but without
    /// depending on the caller's numbering being globally monotonic.
    next_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Channel to the background parse thread.  `None` only after [`Self::new`]
    /// fails to spawn the thread (then everything falls back to inline parsing).
    tx: Option<std::sync::mpsc::Sender<PrimeRequest>>,
    /// Join handle for graceful shutdown on drop.
    worker: Option<std::thread::JoinHandle<()>>,
}

/// A unit of background parse work shipped to the worker thread.
struct PrimeRequest {
    /// Snapshot to rebuild from (carry-forward source for already-parsed entries).
    base: Arc<MarkdownCache>,
    /// Complete live job set for this dispatch. Each job carries its own
    /// scoped token set, so the rebuild needs no ambient tokens.
    jobs: Vec<PrimeJob>,
    /// Internally-generated epoch this rebuild targets (store is gated on it).
    epoch: u64,
}

impl Default for MarkdownPrimer {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkdownPrimer {
    /// Create a primer with an empty cache and a running background parse thread.
    pub(crate) fn new() -> Self {
        let cache = Arc::new(arc_swap::ArcSwap::from_pointee(MarkdownCache::new()));
        let published_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let next_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (tx, rx) = std::sync::mpsc::channel::<PrimeRequest>();

        let worker_cache = Arc::clone(&cache);
        let worker_epoch = Arc::clone(&published_epoch);
        let worker = std::thread::Builder::new()
            .name("md-prime".to_string())
            .spawn(move || {
                // Each request rebuilds a complete snapshot off the commit thread
                // and swaps it in — unless a newer snapshot was published in the
                // meantime (stale-clobber guard).
                while let Ok(req) = rx.recv() {
                    let next = MarkdownCache::rebuild_from(&req.base, &req.jobs);
                    publish_if_newer(&worker_cache, &worker_epoch, Arc::new(next), req.epoch);
                }
            })
            .ok();

        Self {
            cache,
            published_epoch,
            next_epoch,
            tx: tx_if_worker_spawned(tx, &worker),
            worker,
        }
    }

    /// Load the current cache snapshot lock-free.
    ///
    /// Call this on the render path; it is a single atomic pointer load.  The
    /// returned [`Arc`] keeps the snapshot alive for as long as the caller holds
    /// it, so a concurrent swap on the background thread cannot free it
    /// mid-read.
    #[inline]
    pub(crate) fn load(&self) -> Arc<MarkdownCache> {
        self.cache.load_full()
    }

    /// Prime the cache for `jobs` (the complete live `TextMarkdownNode` set for
    /// the current scene state), parsing any missing content off the commit
    /// thread when it is large.
    ///
    /// Behaviour:
    /// - If nothing is missing and the live set already matches the current
    ///   snapshot, this is a no-op.
    /// - If the only change is removals (dead nodes), a pruned snapshot is built
    ///   inline (eviction is cheap — no parsing) and stored.
    /// - If missing content is small (≤ [`INLINE_PARSE_BYTE_THRESHOLD`] total),
    ///   it is parsed inline and stored.
    /// - Otherwise the rebuild is dispatched to the background parse thread; the
    ///   render path's cache-miss fallback keeps frames correct until the swap
    ///   lands.
    ///
    /// Every call claims a fresh internal epoch up front (see the note
    /// on [`MarkdownPrimer`]) so the stale-clobber guard orders purely by this
    /// primer's own call sequence, never by caller-supplied scene numbering.
    pub(crate) fn prime(&self, jobs: Vec<PrimeJob>) {
        let epoch = self
            .next_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;

        let current = self.cache.load();

        // Compute the byte volume of content not yet present in the snapshot,
        // and whether the live set already matches the snapshot exactly.  A job
        // carrying `content: Some(_)` is one the caller already determined is a
        // cache miss; we sum those bytes to size the parse work.
        let mut missing_bytes: usize = 0;
        for job in &jobs {
            if let Some(content) = &job.content {
                if current.get_by_key(&job.key).is_none() {
                    missing_bytes += content.len();
                }
            }
        }
        let exact_match = missing_bytes == 0 && current.len() == distinct_key_count(&jobs);

        if exact_match {
            // Snapshot already correct for this live set — nothing to do.
            // Still advance the published epoch so a later in-flight rebuild
            // from an earlier dispatch cannot clobber the current good snapshot.
            bump_version_to(&self.published_epoch, epoch);
            return;
        }

        // Removals-only (or small parse): rebuild inline.  Building a pruned or
        // small snapshot is cheap and avoids a thread hop for the common case of
        // a node disappearing or a tiny content edit.
        if missing_bytes <= INLINE_PARSE_BYTE_THRESHOLD || self.tx.is_none() {
            let next = MarkdownCache::rebuild_from(&current, &jobs);
            publish_if_newer(&self.cache, &self.published_epoch, Arc::new(next), epoch);
            return;
        }

        // Large parse: dispatch off the commit thread.  `self.tx` is `Some`
        // here (the `is_none()` case took the inline branch above).
        let req = PrimeRequest {
            base: current.clone(),
            jobs,
            epoch,
        };
        if let Some(tx) = &self.tx {
            if let Err(failed) = tx.send(req) {
                // Worker is gone (shutdown / panic): fall back to an inline
                // rebuild so correctness is preserved even without the thread.
                let req = failed.0;
                let next = MarkdownCache::rebuild_from(&req.base, &req.jobs);
                publish_if_newer(
                    &self.cache,
                    &self.published_epoch,
                    Arc::new(next),
                    req.epoch,
                );
            }
        }
        // We deliberately do not block on completion.  The worker advances the
        // published epoch on store; the render path falls back to inline parse
        // for any node not yet in the snapshot until the swap lands.
    }

    /// Reset the cache to empty and force the next [`Self::prime`] to repopulate.
    ///
    /// Called when the token map changes (parsed output depends on tokens) so
    /// stale styling is never served.  Stores an empty snapshot immediately so
    /// readers see a clean (cache-miss → inline-parse) state until the next
    /// prime lands.
    pub(crate) fn reset(&self) {
        // Claim a fresh epoch so any in-flight background rebuild dispatched
        // before this reset cannot clobber it.
        let epoch = self
            .next_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        publish_if_newer(
            &self.cache,
            &self.published_epoch,
            Arc::new(MarkdownCache::new()),
            epoch,
        );
    }

    /// Whether the current snapshot is empty (test/diagnostic use).
    #[inline]
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.cache.load().is_empty()
    }
}

impl Drop for MarkdownPrimer {
    fn drop(&mut self) {
        // Drop the sender so the worker's `recv()` returns `Err` and the loop
        // exits, then join it.  Without this the thread would linger until
        // process exit (harmless, but untidy in tests that create many primers).
        self.tx.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Count distinct keys in `jobs` (the live set may contain duplicate content).
fn distinct_key_count(jobs: &[PrimeJob]) -> usize {
    let mut keys: Vec<[u8; 32]> = jobs.iter().map(|j| j.key).collect();
    keys.sort_unstable();
    keys.dedup();
    keys.len()
}

/// Store `next` into `cache` iff `epoch` is newer than the last published
/// epoch, then record `epoch` as published.
///
/// This is the stale-clobber guard: a background rebuild that finishes after a
/// newer snapshot has already been published is dropped instead of overwriting
/// the newer data.  Uses a CAS loop so the commit thread and worker thread
/// never lose a newer store to a race.
///
/// `epoch` must come from [`MarkdownPrimer`]'s own internally-generated
/// [`MarkdownPrimer::next_epoch`] counter (see the note on that
/// type), never from caller-supplied scene numbering — the latter is only
/// guaranteed monotonic within one `SceneGraph` instance's lifetime, and a
/// `MarkdownPrimer` reused across instances would silently and permanently
/// drop legitimate results once compared against a lower-numbered new scene.
fn publish_if_newer(
    cache: &arc_swap::ArcSwap<MarkdownCache>,
    published: &std::sync::atomic::AtomicU64,
    next: Arc<MarkdownCache>,
    epoch: u64,
) {
    use std::sync::atomic::Ordering;
    loop {
        let cur = published.load(Ordering::Acquire);
        if epoch < cur {
            // A newer snapshot already won — drop this stale rebuild.
            return;
        }
        // Claim this epoch.  On success, publish.  `epoch == cur` is allowed
        // (re-publish for the same epoch, e.g. removals-only after a parse) so
        // the latest store for an epoch always wins.
        match published.compare_exchange_weak(cur, epoch, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                cache.store(next);
                return;
            }
            Err(_) => continue,
        }
    }
}

/// Advance `published` to at least `epoch` without storing a new snapshot.
fn bump_version_to(published: &std::sync::atomic::AtomicU64, epoch: u64) {
    use std::sync::atomic::Ordering;
    let mut cur = published.load(Ordering::Acquire);
    while epoch > cur {
        match published.compare_exchange_weak(cur, epoch, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(observed) => cur = observed,
        }
    }
}

/// Return the sender only if the worker thread actually spawned; otherwise
/// `None` so [`MarkdownPrimer::prime`] takes the inline-parse path.
fn tx_if_worker_spawned(
    tx: std::sync::mpsc::Sender<PrimeRequest>,
    worker: &Option<std::thread::JoinHandle<()>>,
) -> Option<std::sync::mpsc::Sender<PrimeRequest>> {
    if worker.is_some() { Some(tx) } else { None }
}

// ─── parse_markdown_subset ───────────────────────────────────────────────────

#[cfg(test)]
thread_local! {
    /// Test-only PER-THREAD counter of [`parse_markdown_subset`] invocations, so
    /// tests can prove a code path performs zero markdown parses (the
    /// per-frame flow measurement MUST NOT re-parse cached content). Counting the
    /// actual behavior is stronger than asserting which branch was taken.
    ///
    /// Thread-local (not a global atomic) so it stays correct under cargo's
    /// parallel test execution: measurement is synchronous on the calling test's
    /// thread, and other tests parsing on their own threads cannot perturb the
    /// count this test reads.
    pub(crate) static PARSE_MARKDOWN_SUBSET_CALLS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// Parse the Phase-1 CommonMark subset from `content` into a [`ParsedMarkdown`].
///
/// The parser is intentionally simple and allocation-efficient: it walks the
/// input line by line, then processes inline markup within each line.  It does
/// not implement a full CommonMark spec-compliant parser.
///
/// # Excluded constructs
///
/// Tables, images, raw HTML, blockquotes, footnotes, strikethrough, task lists,
/// and non-HTTPS autolinks are **not** parsed.  Their literal source text is included in
/// the output verbatim so transcript content is never silently dropped.
pub(crate) fn parse_markdown_subset(content: &str, tokens: &MarkdownTokens) -> ParsedMarkdown {
    #[cfg(test)]
    PARSE_MARKDOWN_SUBSET_CALLS.with(|calls| calls.set(calls.get() + 1));
    let mut plain = String::with_capacity(content.len());
    let mut spans: Vec<StyledSpan> = Vec::new();
    // Code-panel backdrop regions: populated when tokens.code_background is Some.
    // Each entry records a contiguous code region in plain-text byte space so the
    // renderer can emit a geometry quad behind that region.
    let mut code_panels: Vec<CodePanelSpan> = Vec::new();
    // Per-list-item hanging-indent metadata.
    let mut list_items: Vec<ListItemSpan> = Vec::new();
    // Thematic-break divider offsets (transcript turn separators).
    let mut thematic_breaks: Vec<usize> = Vec::new();
    // Fenced code block contents (copy targets, hud-ljhku.2).
    let mut code_blocks: Vec<(usize, usize)> = Vec::new();

    let mut in_fenced_block = false;
    let mut fence_char: Option<char> = None;
    let mut fence_marker_len: usize = 0;
    // Track whether the previous output line was non-empty for blank-line
    // separation between blocks.
    let mut prev_was_empty = true;

    let lines: Vec<&str> = content.lines().collect();
    let n = lines.len();
    let mut i = 0;

    while i < n {
        let raw = lines[i];

        // ── Fenced code block (``` or ~~~) ───────────────────────────────
        if let Some((ch, len)) = detect_fence_open(raw) {
            if !in_fenced_block {
                in_fenced_block = true;
                fence_char = Some(ch);
                fence_marker_len = len;
                // Emit blank line separator if needed.
                if !prev_was_empty && !plain.is_empty() {
                    plain.push('\n');
                }
                i += 1;
                // Record panel start before collecting the fence body.
                let panel_start = plain.len();
                // Collect fence body until closing fence or EOF.
                while i < n {
                    let body_line = lines[i];
                    if is_fence_close(body_line, ch, len) {
                        in_fenced_block = false;
                        fence_char = None;
                        fence_marker_len = 0;
                        i += 1;
                        break;
                    }
                    // Emit body as monospace styled span.
                    let start = plain.len();
                    plain.push_str(body_line);
                    plain.push('\n');
                    let end = plain.len() - 1; // exclude trailing newline from span
                    if start < end {
                        spans.push(StyledSpan {
                            start_byte: start,
                            end_byte: end,
                            attr: StyleAttr {
                                weight: None,
                                italic: false,
                                monospace: tokens.code_monospace,
                                // code_color sets the foreground; code_background is a
                                // backdrop panel (not a text color modifier).
                                color: tokens.code_color,
                                background_color: tokens.code_background,
                                size_scale: None,
                                href: None,
                            },
                        });
                    }
                    prev_was_empty = body_line.is_empty();
                    i += 1;
                }
                if plain.len() > panel_start {
                    code_blocks.push((panel_start, plain.len() - 1));
                }
                // Emit one panel record for the entire fenced block (even if
                // it is a multi-line block — the renderer approximates it as a
                // single rect spanning the block's plain-text byte range).
                if tokens.code_background.is_some() && plain.len() > panel_start {
                    code_panels.push(CodePanelSpan {
                        start_byte: panel_start,
                        end_byte: plain.len(),
                        kind: CodePanelKind::Block,
                    });
                }
                continue;
            }
        }

        // ── Indented code block (4-space or tab) ─────────────────────────
        if !in_fenced_block && (raw.starts_with("    ") || raw.starts_with('\t')) {
            let body = if let Some(stripped) = raw.strip_prefix("    ") {
                stripped
            } else {
                &raw[1..]
            };
            if !prev_was_empty && !plain.is_empty() {
                plain.push('\n');
            }
            let start = plain.len();
            plain.push_str(body);
            let end = plain.len();
            if start < end {
                spans.push(StyledSpan {
                    start_byte: start,
                    end_byte: end,
                    attr: StyleAttr {
                        weight: None,
                        italic: false,
                        monospace: tokens.code_monospace,
                        // code_color sets the foreground; code_background is a
                        // backdrop panel (not a text color modifier).
                        color: tokens.code_color,
                        background_color: tokens.code_background,
                        size_scale: None,
                        href: None,
                    },
                });
                if tokens.code_background.is_some() {
                    code_panels.push(CodePanelSpan {
                        start_byte: start,
                        end_byte: end,
                        kind: CodePanelKind::Block,
                    });
                }
            }
            prev_was_empty = body.is_empty();
            i += 1;
            continue;
        }

        // ── Thematic break (---, ***, ___) → transcript turn separator ───
        // Gated on a configured separator color so surfaces without the
        // `portal.divider.color` token keep the prior behavior (a `---` line
        // renders as ordinary text). The break occupies one blank line the
        // renderer draws a token-styled divider quad on.
        if !in_fenced_block && tokens.separator_color.is_some() && is_thematic_break(raw) {
            // Close the previous line so the break sits on its own row.
            if !prev_was_empty && !plain.is_empty() {
                plain.push('\n');
            }
            // Record the divider's (blank) line start, then reserve the line.
            thematic_breaks.push(plain.len());
            plain.push('\n');
            prev_was_empty = true;
            i += 1;
            continue;
        }

        // ── ATX heading (# … ######) ─────────────────────────────────────
        if !in_fenced_block {
            if let Some((level, heading_text)) = parse_atx_heading(raw) {
                // Heading top margin: insert a blank line before the heading when
                // tokens.heading_margin_top ≥ 1.0 and there is preceding content.
                // Values in (0.0, 1.0) are treated as 0 (no blank line) because
                // plain-text output does not support fractional line gaps.
                if !plain.is_empty() {
                    if !prev_was_empty {
                        plain.push('\n');
                    }
                    if tokens.heading_margin_top >= 1.0 && !prev_was_empty {
                        plain.push('\n');
                    }
                }
                let level_idx = (level as usize).saturating_sub(1).min(5);
                let scale = tokens.heading_scale[level_idx];
                let attr = StyleAttr {
                    weight: Some(tokens.heading_weight[level_idx]),
                    italic: false,
                    monospace: false,
                    color: None,
                    background_color: None,
                    // Apply heading scale so the font size actually changes at render time.
                    size_scale: if (scale - 1.0).abs() > f32::EPSILON {
                        Some(scale)
                    } else {
                        None
                    },
                    href: None,
                };
                let start = plain.len();
                let prev_len = spans.len();
                // Process inline markup inside the heading text.
                // base_override propagates heading weight into nested spans.
                let heading_chars: Vec<char> = heading_text.chars().collect();
                process_inline(&heading_chars, &mut plain, &mut spans, tokens, Some(&attr));
                let end = plain.len();
                // Fill only the unstyled *gaps* within [start, end) with the
                // heading base style.  This avoids inserting a wide overlapping
                // span across ranges that already have inner-markup spans.
                if start < end {
                    fill_gaps_with_base(&attr, start, end, prev_len, &mut spans);
                }
                // Heading bottom margin: insert a blank line after the heading when
                // tokens.heading_margin_bottom ≥ 1.0.
                if tokens.heading_margin_bottom >= 1.0 {
                    plain.push('\n');
                    prev_was_empty = true;
                } else {
                    prev_was_empty = false;
                }
                i += 1;
                continue;
            }
        }

        // ── List item (unordered or ordered) ────────────────────────────
        if !in_fenced_block {
            if let Some((indent_spaces, bullet, item_text)) = parse_list_item(raw) {
                // List item spacing: insert a blank line between items when
                // tokens.list_item_spacing ≥ 1.0 and there is a preceding item.
                if !prev_was_empty && !plain.is_empty() {
                    plain.push('\n');
                    if tokens.list_item_spacing >= 1.0 {
                        plain.push('\n');
                    }
                }
                // Record item start byte (before any indent/bullet prefix).
                let item_start_byte = plain.len();
                // Emit indent (2 spaces per level, minimum 0, maximum 4 levels).
                let indent_level = ((indent_spaces / 2) as u8).min(4);
                let indent = "  ".repeat(indent_level as usize);
                plain.push_str(&indent);
                // `bullet` is "• " for unordered or "N. " / "N) " for ordered,
                // preserving the ordinal so the rendered text matches the source.
                plain.push_str(&bullet);
                // Record content start byte (after indent + bullet prefix).
                // The difference (content_start_byte - item_start_byte) gives the
                // hanging-indent width: continuation lines align here, not at the bullet.
                let content_start_byte = plain.len();
                list_items.push(ListItemSpan {
                    item_start_byte,
                    content_start_byte,
                    indent_level,
                });
                let item_chars: Vec<char> = item_text.chars().collect();
                process_inline(&item_chars, &mut plain, &mut spans, tokens, None);
                prev_was_empty = false;
                i += 1;
                continue;
            }
        }

        // ── Blank line ───────────────────────────────────────────────────
        if raw.trim().is_empty() {
            if !prev_was_empty && !plain.is_empty() {
                plain.push('\n');
            }
            prev_was_empty = true;
            i += 1;
            continue;
        }

        // ── Normal paragraph line ────────────────────────────────────────
        if !prev_was_empty && !plain.is_empty() {
            plain.push('\n');
        }
        let line_chars: Vec<char> = raw.chars().collect();
        process_inline(&line_chars, &mut plain, &mut spans, tokens, None);
        prev_was_empty = false;
        i += 1;

        // suppress unused variable warnings for fence variables in non-fenced path
        let _ = fence_char;
        let _ = fence_marker_len;
    }

    // Normalise: sort spans and remove zero-length ones.
    spans.sort_by_key(|s| (s.start_byte, s.end_byte));
    spans.retain(|s| s.start_byte < s.end_byte && s.end_byte <= plain.len());

    // Post-process: build inline code_panels from spans that carry a
    // background_color (set by the inline-code handler below).  Doing this
    // as a post-processing step avoids threading a `&mut Vec<CodePanelSpan>`
    // through all the recursive process_inline_inner calls.
    for span in &spans {
        if span.attr.background_color.is_some() && span.attr.monospace {
            code_panels.push(CodePanelSpan {
                start_byte: span.start_byte,
                end_byte: span.end_byte,
                kind: CodePanelKind::Inline,
            });
        }
    }

    // Links: merge adjacent spans that carry the same target.
    let mut links: Vec<LinkSpan> = Vec::new();
    for span in &spans {
        let Some(href) = &span.attr.href else {
            continue;
        };
        match links.last_mut() {
            Some(last) if last.end_byte == span.start_byte && last.href == *href => {
                last.end_byte = span.end_byte;
            }
            _ => links.push(LinkSpan {
                start_byte: span.start_byte,
                end_byte: span.end_byte,
                href: href.clone(),
            }),
        }
    }

    ParsedMarkdown {
        plain_text: Arc::from(plain),
        spans,
        code_panels,
        list_items,
        thematic_breaks,
        links,
        code_blocks,
        line_height_multiplier: tokens.line_height_multiplier,
    }
}

/// Whether `line` is a CommonMark thematic break: three or more matching `-`,
/// `*`, or `_` characters, optionally separated by spaces.
///
/// Used to render transcript turn separators. Kept intentionally simple (the
/// leading-indent and internal-space allowances of full CommonMark are a
/// superset of what the transcript lowering emits).
fn is_thematic_break(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let mut marker: Option<char> = None;
    let mut count = 0usize;
    for c in trimmed.chars() {
        if c.is_whitespace() {
            continue;
        }
        if c != '-' && c != '*' && c != '_' {
            return false;
        }
        match marker {
            None => {
                marker = Some(c);
                count = 1;
            }
            Some(m) if m == c => count += 1,
            Some(_) => return false, // mixed markers are not a thematic break
        }
    }
    count >= 3
}

// ─── Inline processing ───────────────────────────────────────────────────────

/// Maximum nesting depth for inline markup recursion.
///
/// Beyond this depth `process_inline_inner` emits remaining characters as
/// literal text rather than recursing, bounding stack consumption to a safe
/// constant regardless of how deeply adversarial input nests brackets or
/// emphasis markers.  A 64 KiB payload with only `[` characters fits ~32k
/// nesting levels; this cap stops recursion well before the ~1 MiB stack
/// limit on the Windows main thread.
const MAX_INLINE_DEPTH: usize = 100;

/// Process inline markdown constructs within a single paragraph/heading/list
/// item `chars` and append the result to `out`, emitting styled spans.
///
/// `base_override` is a style that has already been applied to the containing
/// block (e.g. heading weight); inline markup *adds* to it.
///
/// Callers that start from a `&str` should convert once with
/// `.chars().collect::<Vec<char>>()` and pass the resulting slice.
/// Recursive calls pass sub-slices of the already-collected `Vec<char>`,
/// eliminating the per-call allocation that the original `text: &str`
/// approach incurred.
///
/// This is the public entry point; it delegates to [`process_inline_inner`]
/// with `depth = 0`.
fn process_inline(
    chars: &[char],
    out: &mut String,
    spans: &mut Vec<StyledSpan>,
    tokens: &MarkdownTokens,
    base_override: Option<&StyleAttr>,
) {
    // Build the bracket-match table once for this slice so that the link
    // branch can resolve `[…]` in O(1) instead of scanning to end-of-input
    // per opening bracket.  Total cost: O(chars.len()).  Skip the allocation
    // entirely when there are no brackets on the line.
    let bracket_matches = if chars.contains(&'[') {
        build_bracket_matches(chars)
    } else {
        Vec::new()
    };
    // Build the paren-match table once for this slice so that the link-dest
    // branch can resolve `(…)` in O(1) instead of scanning to end-of-input
    // per opening `(`.  Total cost: O(chars.len()).  Skip the allocation
    // entirely when there are no parens on the line.
    let paren_matches = if chars.contains(&'(') {
        build_paren_matches(chars)
    } else {
        Vec::new()
    };
    process_inline_inner(
        chars,
        out,
        spans,
        tokens,
        base_override,
        0,
        &bracket_matches,
        &paren_matches,
    );
}

/// Inner recursive worker for inline processing.
///
/// `depth` is the current nesting level.  When it reaches [`MAX_INLINE_DEPTH`]
/// all remaining characters are emitted as literals — the parser degrades
/// gracefully instead of overflowing the stack.
///
/// `bracket_matches[i]` is `Some(j)` when `chars[i] == '['` and `j` is the
/// index of the matching `']'` (computed once by the top-level caller).  The
/// slice is valid for the *full* original `chars` only.  Recursive calls
/// operate on sub-slices and pass an empty slice for `bracket_matches`;
/// link-text sub-slice calls rebuild the bracket table for the sub-slice
/// immediately before the recursive call; emphasis sub-slice calls pass `&[]`
/// directly (no nested link detection inside emphasis spans is needed).
///
/// `paren_matches[i]` is `Some(j)` when `chars[i] == '('` and `j` is the
/// index of the matching `')'` (computed once by the top-level caller).  Same
/// scope rules as `bracket_matches`: link-text sub-slice calls rebuild the
/// paren table for the sub-slice before the recursive call; emphasis sub-slice
/// calls pass `&[]` (no paren-flood risk inside emphasis spans).
///
/// # Argument count
///
/// The eight parameters are all contextual state that must be threaded through
/// the recursion: chars, output, spans, tokens, base_override, depth, and two
/// precomputed match tables (bracket and paren).  A context struct would add
/// indirection with no readability benefit at this call frequency; the lint is
/// suppressed instead.
#[allow(clippy::too_many_arguments)]
fn process_inline_inner(
    chars: &[char],
    out: &mut String,
    spans: &mut Vec<StyledSpan>,
    tokens: &MarkdownTokens,
    base_override: Option<&StyleAttr>,
    depth: usize,
    bracket_matches: &[Option<usize>],
    paren_matches: &[Option<usize>],
) {
    // Depth cap: emit all remaining characters as literals so adversarial
    // deeply-nested input cannot overflow the stack.
    if depth >= MAX_INLINE_DEPTH {
        for &c in chars {
            out.push(c);
        }
        return;
    }

    let n = chars.len();
    let mut i = 0;

    // Memo of emphasis-close failures, keyed by (marker, count).  A scan for a
    // closing run starting at `from` searches a *suffix*; if it fails, every
    // later start (a shorter suffix) for the same (marker, count) also fails.
    // We record the lowest `from` known to have no close and short-circuit
    // future scans, turning the otherwise-O(n²) marker flood (`*`×65535) into
    // amortized O(n).  This is a pure short-circuit — it never changes which
    // emphasis spans match.
    let mut emphasis_close_fail = EmphasisCloseMemo::new();
    // Memo of backtick-close failures, keyed by tick_count.  Same principle as
    // EmphasisCloseMemo: if no closing run of length `tick_count` exists from
    // position `from`, no later position can succeed either.  Converts the
    // `` ` ``×65534 flood (O(n²)) into amortized O(n).
    let mut backtick_close_fail = BacktickCloseMemo::new();

    while i < n {
        let ch = chars[i];

        // ── Images: `![alt](url)` → excluded, render full literal source ──
        // Per contract: excluded constructs are never silently dropped or
        // transformed — emit the verbatim source substring so agents can
        // see exactly what was in the input.
        if ch == '!' && i + 1 < n && chars[i + 1] == '[' {
            if let Some(end) =
                find_link_end_with_table(chars, i + 1, bracket_matches, paren_matches)
            {
                // Emit the full `![alt](url)` construct verbatim.
                for &c in &chars[i..=end] {
                    out.push(c);
                }
                i = end + 1;
                continue;
            }
            // No matching `](...)` — emit the `!` literally and let the `[`
            // branch handle the rest on the next iteration.
            out.push(ch);
            i += 1;
            continue;
        }

        // ── Links: `[text](url)` → styled text, no navigation ────────────
        if ch == '[' {
            // Use the precomputed bracket-match table for O(1) lookup.
            let bracket_close = if i < bracket_matches.len() {
                bracket_matches[i]
            } else {
                None
            };
            if let Some(bracket_close) = bracket_close {
                if bracket_close + 1 < n && chars[bracket_close + 1] == '(' {
                    if let Some(paren_close) =
                        find_paren_close(chars, bracket_close + 1, paren_matches)
                    {
                        let start = out.len();
                        let prev_len = spans.len();
                        // Build a bracket-match table for the sub-slice so nested
                        // links inside link text (e.g. `**[x](u)**`) are parsed
                        // correctly by the recursive call.
                        let inner_slice = &chars[i + 1..bracket_close];
                        let inner_bm = if inner_slice.contains(&'[') {
                            build_bracket_matches(inner_slice)
                        } else {
                            Vec::new()
                        };
                        // Build a paren-match table for the inner sub-slice so
                        // nested link destinations inside link text (e.g.
                        // `[text with [x](u)](outer)`) are resolved in O(1)
                        // instead of O(n) per failing `(`.  Without this table,
                        // inputs like `[` + `[](` × 21841 + `](u)` trigger
                        // O(n²) fallback scans inside the recursive call.
                        let inner_pm = if inner_slice.contains(&'(') {
                            build_paren_matches(inner_slice)
                        } else {
                            Vec::new()
                        };
                        // Only an HTTPS target with a host is a link; any other
                        // scheme renders its text plain (hud-ljhku.2).
                        let dest: String = chars[bracket_close + 2..paren_close].iter().collect();
                        let href = tze_hud_scene::https_link_target(dest.trim()).map(Arc::from);
                        let mut link_attr = base_override.cloned().unwrap_or_else(StyleAttr::plain);
                        if href.is_some() && tokens.link_color.is_some() {
                            link_attr.color = tokens.link_color;
                        }
                        link_attr.href = href.clone().or(link_attr.href);
                        process_inline_inner(
                            inner_slice,
                            out,
                            spans,
                            tokens,
                            Some(&link_attr),
                            depth + 1,
                            &inner_bm,
                            &inner_pm,
                        );
                        let end = out.len();
                        // Use fill_gaps_with_base to cover unstyled gaps with the
                        // link style, keeping spans non-overlapping.
                        if start < end && link_attr != StyleAttr::plain() {
                            fill_gaps_with_base(&link_attr, start, end, prev_len, spans);
                        }
                        if let Some(href) = &href {
                            for span in &mut spans[prev_len..] {
                                if span.attr.href.is_none() {
                                    span.attr.href = Some(href.clone());
                                    if tokens.link_color.is_some() {
                                        span.attr.color = tokens.link_color;
                                    }
                                }
                            }
                        }
                        i = paren_close + 1;
                        continue;
                    }
                }
            }
            // No match → emit literally.
            out.push(ch);
            i += 1;
            continue;
        }

        // ── Autolinks `<https://…>` and bare `https://…` (hud-ljhku.2) ─────
        if let Some((text_start, text_end, next)) = autolink_at(chars, i) {
            let text: String = chars[text_start..text_end].iter().collect();
            if let Some(href) = tze_hud_scene::https_link_target(&text) {
                let start = out.len();
                out.push_str(&text);
                let mut attr = base_override.cloned().unwrap_or_else(StyleAttr::plain);
                if tokens.link_color.is_some() {
                    attr.color = tokens.link_color;
                }
                attr.href = Some(Arc::from(href));
                spans.push(StyledSpan {
                    start_byte: start,
                    end_byte: out.len(),
                    attr,
                });
                i = next;
                continue;
            }
        }

        // ── Inline code: `code` ──────────────────────────────────────────
        if ch == '`' {
            // Count opening backtick run.
            let tick_start = i;
            let mut tick_count = 1;
            while i + tick_count < n && chars[i + tick_count] == '`' {
                tick_count += 1;
            }
            // Find matching closing run via memo to avoid O(n²) on backtick floods.
            if let Some(close_start) =
                backtick_close_fail.find(chars, tick_start + tick_count, tick_count)
            {
                let start = out.len();
                for &c in &chars[tick_start + tick_count..close_start] {
                    out.push(c);
                }
                let end = out.len();
                if start < end {
                    spans.push(StyledSpan {
                        start_byte: start,
                        end_byte: end,
                        attr: StyleAttr {
                            weight: base_override.and_then(|b| b.weight),
                            italic: false,
                            monospace: tokens.code_monospace,
                            // code_color sets the text foreground; code_background
                            // is a backdrop panel rendered behind the code text —
                            // it must NOT bleed into the text color.
                            color: tokens
                                .code_color
                                .or_else(|| base_override.and_then(|b| b.color)),
                            background_color: tokens.code_background,
                            size_scale: base_override.and_then(|b| b.size_scale),
                            href: None,
                        },
                    });
                }
                i = close_start + tick_count;
                continue;
            }
            // No closing backtick — emit the entire opening run literally and
            // advance past all of it.  CommonMark defines a backtick string as
            // an indivisible token: a run of N backticks preceded by a non-
            // backtick cannot produce a valid shorter-length match from an
            // interior position.  Advancing by tick_count (instead of 1)
            // eliminates the O(n²) re-count of the same run from every internal
            // position — critical for adversarial inputs like `a` + `` ` ``×65534.
            for _ in 0..tick_count {
                out.push('`');
            }
            i += tick_count;
            continue;
        }

        // ── Bold+italic: ***text*** or ___text___ ────────────────────────
        if (ch == '*' || ch == '_') && i + 2 < n && chars[i + 1] == ch && chars[i + 2] == ch {
            let marker = ch;
            let open_end = i + 3;
            if let Some(close_start) = emphasis_close_fail.find(chars, open_end, marker, 3) {
                let start = out.len();
                let prev_len = spans.len();
                // Bold weight comes from token (typography.bold.weight), not hardcoded.
                let base_weight = base_override.and_then(|b| b.weight).unwrap_or(400);
                let inner_attr = StyleAttr {
                    weight: Some(base_weight.max(tokens.bold_weight)),
                    italic: true,
                    monospace: base_override.map(|b| b.monospace).unwrap_or(false),
                    color: base_override.and_then(|b| b.color),
                    background_color: None,
                    size_scale: base_override.and_then(|b| b.size_scale),
                    href: None,
                };
                process_inline_inner(
                    &chars[open_end..close_start],
                    out,
                    spans,
                    tokens,
                    Some(&inner_attr),
                    depth + 1,
                    &[],
                    &[],
                );
                let end = out.len();
                // Fill only unstyled gaps to avoid overlapping spans.
                if start < end {
                    fill_gaps_with_base(&inner_attr, start, end, prev_len, spans);
                }
                i = close_start + 3;
                continue;
            }
        }

        // ── Bold: **text** or __text__ ───────────────────────────────────
        if (ch == '*' || ch == '_') && i + 1 < n && chars[i + 1] == ch {
            let marker = ch;
            let open_end = i + 2;
            if let Some(close_start) = emphasis_close_fail.find(chars, open_end, marker, 2) {
                let start = out.len();
                let prev_len = spans.len();
                // Bold weight comes from token (typography.bold.weight), not hardcoded.
                let base_weight = base_override.and_then(|b| b.weight).unwrap_or(400);
                let bold_attr = StyleAttr {
                    weight: Some(base_weight.max(tokens.bold_weight)),
                    italic: base_override.map(|b| b.italic).unwrap_or(false),
                    monospace: base_override.map(|b| b.monospace).unwrap_or(false),
                    color: base_override.and_then(|b| b.color),
                    background_color: None,
                    size_scale: base_override.and_then(|b| b.size_scale),
                    href: None,
                };
                process_inline_inner(
                    &chars[open_end..close_start],
                    out,
                    spans,
                    tokens,
                    Some(&bold_attr),
                    depth + 1,
                    &[],
                    &[],
                );
                let end = out.len();
                // Fill only unstyled gaps to avoid overlapping spans.
                if start < end {
                    fill_gaps_with_base(&bold_attr, start, end, prev_len, spans);
                }
                i = close_start + 2;
                continue;
            }
        }

        // ── Italic: *text* or _text_ ─────────────────────────────────────
        if ch == '*' || ch == '_' {
            let marker = ch;
            let open_end = i + 1;
            // Avoid matching lone _ in the middle of a word for `_`.
            let is_word_boundary = if marker == '_' {
                // Simple heuristic: only open if preceded by space/SOL or followed by non-space.
                i == 0 || chars[i - 1].is_whitespace() || chars[i - 1].is_ascii_punctuation()
            } else {
                true
            };
            if is_word_boundary {
                if let Some(close_start) = emphasis_close_fail.find(chars, open_end, marker, 1) {
                    let start = out.len();
                    let prev_len = spans.len();
                    let italic_attr = StyleAttr {
                        weight: base_override.and_then(|b| b.weight),
                        italic: true,
                        monospace: base_override.map(|b| b.monospace).unwrap_or(false),
                        color: base_override.and_then(|b| b.color),
                        background_color: None,
                        size_scale: base_override.and_then(|b| b.size_scale),
                        href: None,
                    };
                    process_inline_inner(
                        &chars[open_end..close_start],
                        out,
                        spans,
                        tokens,
                        Some(&italic_attr),
                        depth + 1,
                        &[],
                        &[],
                    );
                    let end = out.len();
                    // Fill only unstyled gaps to avoid overlapping spans.
                    if start < end {
                        fill_gaps_with_base(&italic_attr, start, end, prev_len, spans);
                    }
                    i = close_start + 1;
                    continue;
                }
            }
        }

        // ── Everything else — emit literally ─────────────────────────────
        out.push(ch);
        i += 1;
    }
}

// ─── Span helpers ─────────────────────────────────────────────────────────────

/// Fill the unstyled byte *gaps* within `[block_start, block_end)` with
/// `base_attr`, appending the new spans to `spans`.
///
/// This is the non-overlapping alternative to inserting a single wide "base"
/// span that would overlap all inner spans.  The caller must have already
/// emitted all inner spans for the block via [`process_inline_inner`]; those
/// spans begin at index `prev_len` in `spans`.
///
/// `prev_len` is the length of `spans` **before** the inner call; this
/// confines the scan to `spans[prev_len..]`, making the function O(new_spans)
/// rather than O(all_spans), which is critical for span-dense adversarial
/// content.
/// An HTTPS autolink at `i`: `<https://…>` (angle brackets dropped) or a bare
/// `https://…` at a word start, ending before whitespace, `<`, or trailing
/// punctuation. Returns the URL char range and the index after the construct.
fn autolink_at(chars: &[char], i: usize) -> Option<(usize, usize, usize)> {
    const SCHEME: [char; 8] = ['h', 't', 't', 'p', 's', ':', '/', '/'];
    let scheme_at = |j: usize| chars.get(j..j + SCHEME.len()) == Some(&SCHEME[..]);
    if chars[i] == '<' && scheme_at(i + 1) {
        let close = chars[i + 1..]
            .iter()
            .take(tze_hud_scene::portal_reading::MAX_LINK_BYTES + 1)
            .position(|&c| c == '>' || c.is_whitespace() || c == '<')?;
        return (chars[i + 1 + close] == '>').then_some((i + 1, i + 1 + close, i + 2 + close));
    }
    // A word start: after whitespace or opening punctuation, never inside a
    // longer token (so `x/https://…` or a `https://https://…` run is no link).
    let word_start = i == 0
        || chars[i - 1].is_whitespace()
        || matches!(chars[i - 1], '(' | '[' | '{' | '"' | '\'' | '*' | '_' | '~');
    if chars[i] != 'h' || !scheme_at(i) || !word_start {
        return None;
    }
    // Bounded like the angle form: a run longer than any valid link is never
    // one, so repeated `https://` runs cost linear, not quadratic, time.
    let limit = tze_hud_scene::portal_reading::MAX_LINK_BYTES + 1;
    let mut end = i + match chars[i..]
        .iter()
        .take(limit)
        .position(|&c| c.is_whitespace() || c == '<')
    {
        Some(len) => len,
        None if chars.len() - i < limit => chars.len() - i,
        None => return None,
    };
    while end > i
        && matches!(
            chars[end - 1],
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | '\'' | '"'
        )
    {
        end -= 1;
    }
    Some((i, end, end))
}

fn fill_gaps_with_base(
    base_attr: &StyleAttr,
    block_start: usize,
    block_end: usize,
    prev_len: usize,
    spans: &mut Vec<StyledSpan>,
) {
    // Collect byte ranges from only the spans added during the inner call.
    // This is O(new_spans) regardless of total spans in the vec.
    // Nested constructs (e.g. links containing emphasis) can push spans in
    // non-start-order; sort so the gap-fill loop is correct.
    let mut inner_ranges: Vec<(usize, usize)> = spans[prev_len..]
        .iter()
        .filter(|s| s.start_byte >= block_start && s.end_byte <= block_end)
        .map(|s| (s.start_byte, s.end_byte))
        .collect();
    inner_ranges.sort_unstable_by_key(|&(s, _)| s);

    // Walk the block range and emit a base-style span for each gap not covered
    // by an inner span.
    let mut cursor = block_start;
    for (s, e) in &inner_ranges {
        if cursor < *s {
            spans.push(StyledSpan {
                start_byte: cursor,
                end_byte: *s,
                attr: base_attr.clone(),
            });
        }
        cursor = cursor.max(*e);
    }
    // Trailing gap.
    if cursor < block_end {
        spans.push(StyledSpan {
            start_byte: cursor,
            end_byte: block_end,
            attr: base_attr.clone(),
        });
    }
}

// ─── Parser helpers ──────────────────────────────────────────────────────────

/// Detect an ATX heading opening (`# ` through `###### `).
/// Returns `(level, heading_text)` or `None`.
fn parse_atx_heading(line: &str) -> Option<(u8, &str)> {
    if !line.starts_with('#') {
        return None;
    }
    let mut level = 0u8;
    let mut rest = line;
    while rest.starts_with('#') && level < 6 {
        level += 1;
        rest = &rest[1..];
    }
    if level == 0 {
        return None;
    }
    // Heading must be followed by a space or be empty.
    if rest.is_empty() {
        return Some((level, ""));
    }
    if let Some(stripped) = rest.strip_prefix(' ') {
        let text = stripped.trim_end_matches('#').trim_end();
        return Some((level, text));
    }
    None
}

/// Detect a list item (ordered or unordered, with leading indent).
///
/// Returns `(indent_spaces, bullet, item_text)` or `None`, where:
/// - `indent_spaces` — number of leading space characters,
/// - `bullet` — the prefix string to emit (`"• "` for unordered; `"N. "` or
///   `"N) "` for ordered, preserving the original ordinal number),
/// - `item_text` — the item body text after the list marker.
fn parse_list_item(line: &str) -> Option<(usize, String, &str)> {
    // Count leading spaces.
    let trimmed_start = line.len() - line.trim_start().len();
    let rest = line.trim_start();

    // Unordered: starts with `- `, `* `, or `+ `.
    // `&rest[2..]` is safe even when `rest.len() == 2` (returns "").
    if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ") {
        return Some((trimmed_start, "• ".to_string(), &rest[2..]));
    }

    // Ordered: starts with digits followed by `.` or `)` and a space.
    // Preserve the ordinal and punctuation so "1. first" renders as "1. first",
    // not "• first".
    let mut digit_count = 0;
    for ch in rest.chars() {
        if ch.is_ascii_digit() {
            digit_count += 1;
        } else {
            break;
        }
    }
    if digit_count > 0 && digit_count <= 9 {
        let after_digits = &rest[digit_count..];
        if let Some(item_text) = after_digits.strip_prefix(". ") {
            // "1. item" → bullet "1. "
            let bullet = format!("{}. ", &rest[..digit_count]);
            return Some((trimmed_start, bullet, item_text));
        }
        if let Some(item_text) = after_digits.strip_prefix(") ") {
            // "1) item" → bullet "1) "
            let bullet = format!("{}) ", &rest[..digit_count]);
            return Some((trimmed_start, bullet, item_text));
        }
    }

    None
}

/// Detect a fenced code block opening line (```` ``` ```` or `~~~`).
/// Returns `(fence_char, fence_len)` or `None`.
fn detect_fence_open(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        let count = trimmed.chars().take_while(|&c| c == '`').count();
        if count >= 3 {
            return Some(('`', count));
        }
    }
    if trimmed.starts_with("~~~") {
        let count = trimmed.chars().take_while(|&c| c == '~').count();
        if count >= 3 {
            return Some(('~', count));
        }
    }
    None
}

/// Return `true` if `line` is a closing fence for a block opened with `(fence_char, fence_len)`.
fn is_fence_close(line: &str, fence_char: char, fence_len: usize) -> bool {
    let trimmed = line.trim();
    let count = trimmed.chars().take_while(|&c| c == fence_char).count();
    count >= fence_len && trimmed.chars().skip(count).all(|c| c.is_whitespace())
}

/// Build a bracket-match table for `chars` in a single O(n) pass.
///
/// Returns a `Vec<Option<usize>>` of length `chars.len()`.  Entry `i` is
/// `Some(j)` when `chars[i] == '['` and `j` is the index of its matching
/// `']'`; all other entries are `None`.  Unmatched `[` (no closing `]`) map
/// to `None`.
///
/// Building this table once per line lets the link branch look up bracket
/// matches in O(1) instead of scanning to end-of-input per opening `[`.
/// Without it, `[`×65535 triggers ~2 × 10⁹ character comparisons; with it,
/// the whole line costs one O(n) pass.
fn build_bracket_matches(chars: &[char]) -> Vec<Option<usize>> {
    // Fast path: no `[` means no bracket pairs — skip the allocation entirely.
    if !chars.contains(&'[') {
        return Vec::new();
    }
    let n = chars.len();
    let mut table = vec![None; n];
    // Stack of open-bracket positions waiting for their closing `]`.
    let mut stack: Vec<usize> = Vec::new();
    for (i, &ch) in chars.iter().enumerate() {
        match ch {
            '[' => stack.push(i),
            ']' => {
                if let Some(open) = stack.pop() {
                    table[open] = Some(i);
                }
            }
            _ => {}
        }
    }
    table
}

/// Find the index of the closing `]` for a `[` at `open_pos` in `chars`.
///
/// This is the fallback O(n) scan used when no precomputed table is available
/// (e.g. for image constructs whose sub-slice does not carry a table).
fn find_bracket_close(chars: &[char], open_pos: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, &ch) in chars.iter().enumerate().skip(open_pos) {
        if ch == '[' {
            depth += 1;
        } else if ch == ']' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Build a paren-match table for `chars` in a single O(n) pass.
///
/// Returns a `Vec<Option<usize>>` of length `chars.len()`.  Entry `i` is
/// `Some(j)` when `chars[i] == '('` and `j` is the index of its matching
/// `')'`; all other entries are `None`.  Unmatched `(` (no closing `)`) map
/// to `None`.
///
/// Building this table once per line lets the link branch look up paren
/// matches in O(1) instead of scanning to end-of-input per opening `(`.
/// Without it, `[](`×21845 triggers ~10⁹ character comparisons; with it,
/// the whole line costs one O(n) pass.
fn build_paren_matches(chars: &[char]) -> Vec<Option<usize>> {
    // Caller (`process_inline`) has already checked `chars.contains(&'(')`;
    // we only run here when at least one `(` is present.
    let n = chars.len();
    let mut table = vec![None; n];
    // Stack of open-paren positions waiting for their closing `)`.
    let mut stack: Vec<usize> = Vec::new();
    for (i, &ch) in chars.iter().enumerate() {
        match ch {
            '(' => stack.push(i),
            ')' => {
                if let Some(open) = stack.pop() {
                    table[open] = Some(i);
                }
            }
            _ => {}
        }
    }
    table
}

/// Find the index of the closing `)` for a `(` at `open_pos` in `chars`,
/// using the precomputed paren-match table when available.
///
/// This is the O(1) fast path when `paren_matches` is non-empty.  Falls back
/// to the O(n) stack scan when the table is empty (e.g. sub-slice calls that
/// were not pre-allocated), preserving correctness in all cases.
fn find_paren_close(
    chars: &[char],
    open_pos: usize,
    paren_matches: &[Option<usize>],
) -> Option<usize> {
    if chars.get(open_pos) != Some(&'(') {
        return None;
    }
    // Fast path: table lookup when available.
    if open_pos < paren_matches.len() {
        return paren_matches[open_pos];
    }
    // Fallback: O(n) depth scan (used for sub-slices without a prebuilt table).
    let mut depth = 0usize;
    for (i, &ch) in chars.iter().enumerate().skip(open_pos) {
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Find the end of a `![alt](url)` image construct using the precomputed
/// bracket-match and paren-match tables.  Returns the position of the final
/// `)`, or `None`.
///
/// Falls back to [`find_bracket_close`] / O(n) depth scan when the respective
/// table is empty (recursive sub-slice calls), preserving correctness.
fn find_link_end_with_table(
    chars: &[char],
    open_bracket: usize,
    bracket_matches: &[Option<usize>],
    paren_matches: &[Option<usize>],
) -> Option<usize> {
    let bracket_close = if open_bracket < bracket_matches.len() {
        bracket_matches[open_bracket]?
    } else {
        find_bracket_close(chars, open_bracket)?
    };
    if bracket_close + 1 < chars.len() && chars[bracket_close + 1] == '(' {
        find_paren_close(chars, bracket_close + 1, paren_matches)
    } else {
        None
    }
}

/// Find the closing backtick run of `tick_count` backticks starting search at `from`.
///
/// This is the O(n) base scanner.  Prefer calling it through
/// [`BacktickCloseMemo::find`] to get amortized-O(1) failure short-circuiting.
fn find_backtick_close(chars: &[char], from: usize, tick_count: usize) -> Option<usize> {
    let n = chars.len();
    let mut i = from;
    while i + tick_count <= n {
        if chars[i] == '`' {
            let mut run = 0;
            while i + run < n && chars[i + run] == '`' {
                run += 1;
            }
            if run == tick_count {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// Amortized-O(1) failure memo around [`find_backtick_close`].
///
/// A scan for a closing backtick run of length `tick_count` starting at `from`
/// examines the suffix `chars[from..]`.  If that scan fails, any later start
/// `from' >= from` (a shorter suffix) for the same `tick_count` also fails.
/// This memo records, per `tick_count`, the lowest `from` already proven to
/// have no close, and short-circuits future failing scans.
///
/// This converts the pathological `` ` ``×65534 flood — where the original code
/// re-scanned to end-of-input for every backtick position, costing O(n²) —
/// into amortized O(n) total.  It is a pure short-circuit: it never changes
/// which code spans match, only how fast failures are detected.
///
/// Backtick runs of length > `MAX_TICK` (32) are handled by a direct scan (no
/// memo entry).  CommonMark spec uses at most 3 backticks; adversarial runs up
/// to 32 are now memoized, closing the residual O(n²) gap for any realistic
/// crafted input.
struct BacktickCloseMemo {
    /// `fail_from[tick_count - 1]` is `Some(f)` when no close run of that
    /// length exists for any start `>= f`.  Indexed by `tick_count - 1`.
    fail_from: [Option<usize>; Self::MAX_TICK],
}

impl BacktickCloseMemo {
    /// Maximum tick-count tracked by the memo.  Runs longer than this fall
    /// back to a direct scan.
    ///
    /// 32 covers all realistic CommonMark inputs (CommonMark spec examples use
    /// at most 3 backticks) and eliminates the O(n²) fallback for adversarial
    /// runs up to 32 backticks long, while remaining stack-allocated (32 ×
    /// 16 bytes = 512 bytes).
    const MAX_TICK: usize = 32;

    fn new() -> Self {
        Self {
            fail_from: [None; Self::MAX_TICK],
        }
    }

    /// Find the closing backtick run, consulting and updating the failure memo.
    fn find(&mut self, chars: &[char], from: usize, tick_count: usize) -> Option<usize> {
        if tick_count == 0 || tick_count > Self::MAX_TICK {
            // Unsupported tick count — fall back to a direct scan.
            return find_backtick_close(chars, from, tick_count);
        }
        let col = tick_count - 1;
        // Short-circuit if a previous scan already proved no close from an
        // earlier (or equal) start position.
        if let Some(f) = self.fail_from[col] {
            if from >= f {
                return None;
            }
        }
        match find_backtick_close(chars, from, tick_count) {
            Some(close) => Some(close),
            None => {
                // Record the lowest failing start for this tick_count.
                // Since we short-circuit when from >= f (above), any existing
                // entry must have f > from, so `from` is always the new
                // minimum — a direct assign is correct and cheaper.
                self.fail_from[col] = Some(from);
                None
            }
        }
    }
}

/// Amortized-O(1) failure memo around [`find_emphasis_close`].
///
/// A scan for a closing emphasis run starting at `from` examines the *suffix*
/// `chars[from..]`.  If that scan finds no close, then any later start
/// `from' >= from` (a shorter suffix) with the same `(marker, count)` also has
/// no close.  This memo records, per `(marker, count)`, the lowest `from`
/// already proven to have no close, and short-circuits future failing scans.
///
/// This converts the pathological `*`×65535 / `_`×65535 marker flood — where
/// the original code re-scanned to end-of-input for every marker position,
/// costing O(n²) — into amortized O(n) total.  It is a pure short-circuit: it
/// never changes which emphasis spans match, only how fast failures are
/// detected.
///
/// Indexing: marker `*` → row 0, `_` → row 1; `count` 1/2/3 → column 0/1/2.
struct EmphasisCloseMemo {
    /// `fail_from[marker_idx][count_idx]` is `Some(f)` when no close exists for
    /// any start `>= f`.
    fail_from: [[Option<usize>; 3]; 2],
}

impl EmphasisCloseMemo {
    fn new() -> Self {
        Self {
            fail_from: [[None; 3]; 2],
        }
    }

    #[inline]
    fn index(marker: char, count: usize) -> Option<(usize, usize)> {
        let row = match marker {
            '*' => 0,
            '_' => 1,
            _ => return None,
        };
        if !(1..=3).contains(&count) {
            return None;
        }
        Some((row, count - 1))
    }

    /// Find the closing run, consulting and updating the failure memo.
    fn find(&mut self, chars: &[char], from: usize, marker: char, count: usize) -> Option<usize> {
        if let Some((row, col)) = Self::index(marker, count) {
            // Short-circuit if a previous scan already proved no close from an
            // earlier (or equal) start position.
            if let Some(f) = self.fail_from[row][col] {
                if from >= f {
                    return None;
                }
            }
            match find_emphasis_close(chars, from, marker, count) {
                Some(close) => Some(close),
                None => {
                    // Record the lowest failing start for this (marker, count).
                    // Since we already short-circuit when from >= f (above), any
                    // existing entry must have f > from, so `from` is always the
                    // new minimum — a direct assign is correct and cheaper.
                    self.fail_from[row][col] = Some(from);
                    None
                }
            }
        } else {
            // Unsupported marker/count — fall back to a direct scan.
            find_emphasis_close(chars, from, marker, count)
        }
    }
}

/// Find the closing emphasis marker (single char `marker` repeated `count` times)
/// starting search at `from`.  Returns the start of the closing marker.
fn find_emphasis_close(chars: &[char], from: usize, marker: char, count: usize) -> Option<usize> {
    let n = chars.len();
    let mut i = from;
    while i + count <= n {
        if chars[i] == marker {
            // Check if exactly `count` markers here (not more).
            let mut run = 0;
            while i + run < n && chars[i + run] == marker {
                run += 1;
            }
            if run == count {
                // For `_`, require word boundary on close side.
                let at_boundary = if marker == '_' {
                    i == 0
                        || chars[i - 1] != ' '
                        || (i + count < n && chars[i + count].is_whitespace())
                } else {
                    true
                };
                if at_boundary {
                    return Some(i);
                }
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

// ─── Color helper ─────────────────────────────────────────────────────────────

/// Convert an sRGB u8 channel value [0, 255] to linear f32 [0.0, 1.0].
///
/// Uses the IEC 61966-2-1 piecewise formula (same standard as wgpu's sRGB conversion).
fn srgb_u8_to_linear(v: u8) -> f32 {
    let s = v as f32 / 255.0;
    if s <= 0.040_45 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// Parse a hex color string (`#RGB`, `#RRGGBB`, `#RRGGBBAA`) into a linear-light [`Rgba`].
///
/// Hex values are interpreted as sRGB; the result is converted to linear for
/// use with glyphon and wgpu.  Returns `None` if the string does not match
/// any supported format.
fn parse_hex_color(s: &str) -> Option<Rgba> {
    let s = s.trim().strip_prefix('#')?;
    // Guard: non-ASCII bytes would make byte-index slicing below unsafe.
    if !s.is_ascii() {
        return None;
    }
    match s.len() {
        3 => {
            let r = u8::from_str_radix(&s[0..1], 16).ok()?;
            let g = u8::from_str_radix(&s[1..2], 16).ok()?;
            let b = u8::from_str_radix(&s[2..3], 16).ok()?;
            Some(Rgba::new(
                srgb_u8_to_linear(r * 17),
                srgb_u8_to_linear(g * 17),
                srgb_u8_to_linear(b * 17),
                1.0,
            ))
        }
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some(Rgba::new(
                srgb_u8_to_linear(r),
                srgb_u8_to_linear(g),
                srgb_u8_to_linear(b),
                1.0,
            ))
        }
        8 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            let a = u8::from_str_radix(&s[6..8], 16).ok()?;
            Some(Rgba::new(
                srgb_u8_to_linear(r),
                srgb_u8_to_linear(g),
                srgb_u8_to_linear(b),
                a as f32 / 255.0,
            ))
        }
        _ => None,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|&(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    #[test]
    fn thematic_break_offsets_follow_the_separator_token() {
        for valid in [
            "---", "***", "___", "----", "- - -", "  ---  ", "* * *", "-- -",
        ] {
            assert!(is_thematic_break(valid), "{valid}");
        }
        for invalid in ["", "--", "**", "---x", "abc", "-*-", "a---", "==="] {
            assert!(!is_thematic_break(invalid), "{invalid}");
        }
        let source = "A\n---\nB\n---\nC";
        let no_overrides = MarkdownTokens::from_token_map(&HashMap::new());
        assert_eq!(
            (
                no_overrides.separator_color,
                no_overrides.separator_thickness_px
            ),
            (None, 1.0)
        );
        let plain = parse_markdown_subset(source, &no_overrides);
        assert_eq!(&*plain.plain_text, source);
        assert!(plain.thematic_breaks.is_empty());

        let themed = MarkdownTokens::from_token_map(&map(&[
            ("portal.divider.color", "#2A3344"),
            ("portal.divider.thickness_px", "2"),
        ]));
        assert_eq!(themed.separator_color, parse_hex_color("#2A3344"));
        assert_eq!(themed.separator_thickness_px, 2.0);
        assert_eq!(tokens().separator_thickness_px, 1.0);
        let divided = parse_markdown_subset(source, &themed);
        let text = &*divided.plain_text;
        assert_eq!(text, "A\n\nB\n\nC");
        assert_eq!(divided.thematic_breaks, [2, 5]);
    }

    #[test]
    fn headings_and_emphasis_style_the_correct_multibyte_ranges() {
        let overrides = MarkdownTokens::from_token_map(&map(&[
            ("typography.heading.1.weight", "900"),
            ("typography.heading.2.scale", "2.0"),
            ("typography.heading.3.weight", "500"),
            ("typography.bold.weight", "800"),
        ]));
        assert_eq!(overrides.heading_weight[0], 900);
        assert_eq!(overrides.heading_weight[1], 700);
        assert_eq!(overrides.heading_weight[2], 500);
        assert_eq!(overrides.heading_scale[1], 2.0);
        assert_eq!(overrides.bold_weight, 800);
        let defaults = MarkdownTokens::from_token_map(&HashMap::new());
        assert_eq!(defaults.heading_weight, tokens().heading_weight);
        assert_eq!(defaults.heading_scale, tokens().heading_scale);
        assert_eq!(defaults.heading_weight[0], 700);
        assert_eq!(defaults.bold_weight, 700);
        for theme in [defaults, overrides] {
            for index in 0..6 {
                let input = format!("{} 見出し", "#".repeat(index + 1));
                let parsed = parse_markdown_subset(&input, &theme);
                assert_eq!(&*parsed.plain_text, "見出し");
                let span = &parsed.spans[0];
                let scale = theme.heading_scale[index];
                assert_eq!(span.attr.weight, Some(theme.heading_weight[index]));
                assert_eq!(span.attr.size_scale, (scale != 1.0).then_some(scale));
                assert_eq!(&parsed.plain_text[span.start_byte..span.end_byte], "見出し");
            }
            for (input, weight, italic) in [
                ("Hello **太字**!", Some(theme.bold_weight), false),
                ("Hello *太字*!", None, true),
                ("Hello ***太字***!", Some(theme.bold_weight), true),
            ] {
                let parsed = parse_markdown_subset(input, &theme);
                assert_eq!(&*parsed.plain_text, "Hello 太字!");
                assert!(
                    parsed.spans.iter().any(|span| {
                        &parsed.plain_text[span.start_byte..span.end_byte] == "太字"
                            && (span.attr.weight, span.attr.italic) == (weight, italic)
                    }),
                    "{input}"
                );
            }
        }
        for text in ["", "plain text 👨‍👩‍👧‍👦"] {
            let plain = parse(text);
            assert_eq!(&*plain.plain_text, text);
            assert!(plain.spans.is_empty());
        }
        for text in [
            "# Title\n\nHello **world** and *folks*.\n\n- item one\n- item two",
            "# 日本語タイトル 👩‍💻\n\n**太字** と *斜体* テキスト。\n\n`コード`",
        ] {
            let mixed = parse(text);
            assert!(!mixed.spans.is_empty());
            for span in &mixed.spans {
                assert!(
                    span.start_byte < span.end_byte
                        && mixed
                            .plain_text
                            .get(span.start_byte..span.end_byte)
                            .is_some(),
                    "{text:?}: invalid span {}..{}",
                    span.start_byte,
                    span.end_byte
                );
            }
        }
    }

    #[test]
    fn transcript_spacing_and_list_metadata_use_configured_and_default_values() {
        let defaults = tokens();
        assert_eq!(
            (
                defaults.line_height_multiplier,
                defaults.heading_margin_top,
                defaults.heading_margin_bottom,
                defaults.list_item_spacing
            ),
            (1.4, 0.0, 0.0, 0.0),
        );
        let compact = parse("Intro\n# Heading\n- first\n* second\n+ third");
        assert_eq!(
            &*compact.plain_text,
            "Intro\nHeading\n• first\n• second\n• third"
        );
        let ordered = parse("1) alpha\n2) beta\n3. gamma");
        assert_eq!(ordered.plain_text.as_ref(), "1) alpha\n2) beta\n3. gamma");
        for (parsed, prefixes) in [
            (compact, ["• ", "• ", "• "]),
            (ordered, ["1) ", "2) ", "3. "]),
        ] {
            assert_eq!(parsed.list_items.len(), prefixes.len());
            for (span, prefix) in parsed.list_items.iter().zip(prefixes) {
                assert_eq!(
                    &parsed.plain_text[span.item_start_byte..span.content_start_byte],
                    prefix
                );
            }
        }
        let themed = MarkdownTokens::from_token_map(&map(&[
            ("typography.line_height.multiplier", "1.6"),
            ("spacing.heading.top", "1"),
            ("spacing.heading.bottom", "1.5"),
            ("spacing.list.item", "1"),
        ]));
        assert_eq!(
            (
                themed.heading_margin_top,
                themed.heading_margin_bottom,
                themed.list_item_spacing
            ),
            (1.0, 1.5, 1.0)
        );
        let parsed = parse_markdown_subset("Intro\n# Heading", &themed);
        assert_eq!(&*parsed.plain_text, "Intro\n\nHeading\n");
        assert_eq!(parsed.line_height_multiplier, 1.6);
        for invalid in ["0.5", "10.0", "NaN"] {
            let settings = map(&[("typography.line_height.multiplier", invalid)]);
            assert_eq!(
                MarkdownTokens::from_token_map(&settings).line_height_multiplier,
                1.4
            );
        }
    }

    #[test]
    fn code_color_backdrops_and_copy_ranges_are_independent() {
        let source = "Use `fmt` and ``か`ん``.\n```sh\nls -la\necho hi\n```";
        let mut theme =
            MarkdownTokens::from_token_map(&map(&[("color.code.background", "#333333")]));
        assert_eq!(theme.code_background, parse_hex_color("#333"));
        for foreground in [None, Some(Rgba::WHITE)] {
            theme.code_color = foreground;
            let parsed = parse_markdown_subset(source, &theme);
            assert_eq!(&*parsed.plain_text, "Use fmt and か`ん.\nls -la\necho hi\n");
            let &[(start, end)] = parsed.code_blocks.as_slice() else {
                panic!("expected exactly one fenced copy range");
            };
            assert_eq!(&parsed.plain_text[start..end], "ls -la\necho hi");
            let monospace: Vec<_> = parsed
                .spans
                .iter()
                .filter(|span| span.attr.monospace)
                .collect();
            for part in ["fmt", "か`ん", "ls -la", "echo hi"] {
                assert!(
                    monospace.iter().any(
                        |span| parsed.plain_text[span.start_byte..span.end_byte].contains(part)
                    ),
                    "{part}"
                );
            }
            for span in monospace {
                assert!(parsed.plain_text.is_char_boundary(span.start_byte));
                assert!(parsed.plain_text.is_char_boundary(span.end_byte));
                assert_eq!(span.attr.color, foreground);
                assert_eq!(span.attr.background_color, theme.code_background);
            }
            let inline: Vec<_> = parsed
                .code_panels
                .iter()
                .filter(|span| matches!(span.kind, CodePanelKind::Inline) && span.end_byte <= start)
                .map(|span| &parsed.plain_text[span.start_byte..span.end_byte])
                .collect();
            assert_eq!(inline, ["fmt", "か`ん"]);
            assert!(
                parsed
                    .code_panels
                    .iter()
                    .any(|span| matches!(span.kind, CodePanelKind::Block)
                        && span.end_byte > span.start_byte)
            );
        }
        assert!(parse(source).code_panels.is_empty());
        let foreground = MarkdownTokens::from_token_map(&map(&[("color.code.text", "#FFFFFF")]));
        let ink_only = parse_markdown_subset(source, &foreground);
        assert!(ink_only.code_panels.is_empty());
        let ink = ink_only
            .spans
            .iter()
            .find(|span| span.attr.monospace)
            .unwrap();
        assert_eq!(ink.attr.color, Some(Rgba::WHITE));
        assert_eq!(ink.attr.background_color, None);
        let indented = parse("    fn hello() {}");
        assert_eq!(&*indented.plain_text, "fn hello() {}");
        assert!(indented.spans.iter().any(|span| span.attr.monospace));
        let three = parse("`a` and `b` and `c`");
        assert_eq!(&*three.plain_text, "a and b and c");
        assert!(
            three
                .spans
                .iter()
                .filter(|span| span.attr.monospace)
                .map(|span| &three.plain_text[span.start_byte..span.end_byte])
                .eq(["a", "b", "c"])
        );
    }

    fn tokens() -> MarkdownTokens {
        MarkdownTokens::default()
    }

    fn parse(s: &str) -> ParsedMarkdown {
        parse_markdown_subset(s, &tokens())
    }

    #[test]
    fn excluded_constructs_render_literally() {
        let cases: &[(&str, &str, &[&str])] = &[
            ("table", "| A | B |\n|---|---|\n| 1 | 2 |", &["|", "A"]),
            ("raw html", "<strong>bold</strong>", &["<"]),
            (
                "blockquote",
                "> This is a blockquote",
                &[">", "This is a blockquote"],
            ),
            ("strikethrough", "~~crossed out~~", &["crossed out", "~~"]),
            ("task list", "- [ ] todo item", &["todo item"]),
        ];
        for (name, input, needles) in cases {
            let md = parse(input);
            for needle in *needles {
                assert!(
                    md.plain_text.contains(needle),
                    "{name}: {needle:?} missing from {:?}",
                    md.plain_text
                );
            }
        }
        assert_eq!(
            &*parse("![diagram](img.png)").plain_text,
            "![diagram](img.png)"
        );
    }

    #[test]
    fn https_link_text_and_destinations_share_exact_byte_ranges() {
        type LinkCase<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);
        let cases: &[LinkCase<'_>] = &[
            (
                "see [the docs](https://example.com/guide) now",
                "see the docs now",
                &[("the docs", "https://example.com/guide")],
            ),
            (
                "[**bold** tail](https://example.com/a)",
                "bold tail",
                &[("bold tail", "https://example.com/a")],
            ),
            (
                "go to https://example.com/x. or <https://example.org>!",
                "go to https://example.com/x. or https://example.org!",
                &[
                    ("https://example.com/x", "https://example.com/x"),
                    ("https://example.org", "https://example.org/"),
                ],
            ),
            ("[doc](fn(arg))", "doc", &[]),
            ("[a](u1) and [b](u2)", "a and b", &[]),
        ];
        for (input, text, spans) in cases {
            let parsed = parse(input);
            assert_eq!(parsed.plain_text.as_ref(), *text);
            let visible = parsed
                .links
                .iter()
                .map(|span| (&text[span.start_byte..span.end_byte], span.href.as_ref()));
            assert!(
                visible.eq(spans.iter().copied()),
                "{input}: {:?}",
                parsed.links
            );
        }
        let long = format!("[x](https://example.com/{})", "a".repeat(2048));
        for input in ["xhttps://example.com", &long] {
            assert!(parse(input).links.is_empty(), "{input}");
        }
        let run = "https://".repeat(5_000);
        let parsed = parse(&run);
        assert!(parsed.links.is_empty());
        assert_eq!(parsed.plain_text.as_ref(), run);
    }

    #[test]
    fn non_https_links_render_plain() {
        let t = MarkdownTokens {
            link_color: Some(Rgba::new(0.0, 0.5, 1.0, 1.0)),
            ..tokens()
        };
        for src in [
            "[a](http://example.com)",
            "[a](file:///etc/passwd)",
            "[a](javascript:alert(1))",
            "http://example.com",
            "ftp://example.com",
        ] {
            let md = parse_markdown_subset(src, &t);
            assert!(md.links.is_empty(), "{src}");
            assert!(
                md.spans.iter().all(|s| s.attr.color.is_none()),
                "{src} is plain"
            );
        }
        assert_eq!(
            &*parse("<http://example.com>").plain_text,
            "<http://example.com>"
        );
    }

    #[test]
    fn adversarial_inputs_terminate_and_preserve_content() {
        #[derive(Clone, Copy)]
        enum Expect {
            /// Every source character is emitted verbatim.
            Verbatim,
            /// Output is non-empty.
            NonEmpty,
            /// Output contains the given text.
            Contains(&'static str),
            /// Output has exactly this many bytes.
            Len(usize),
        }
        let nested = |open: &str, mid: &str, close: &str, n: usize| {
            format!("{}{mid}{}", open.repeat(n), close.repeat(n))
        };
        let cases: Vec<(&str, String, Expect)> = vec![
            ("unmatched '[' flood", "[".repeat(65535), Expect::Len(65535)),
            (
                "nested bold",
                nested("**", "x", "**", 16384),
                Expect::NonEmpty,
            ),
            (
                "nested italic",
                nested("*", "x", "*", 32767),
                Expect::NonEmpty,
            ),
            (
                "nested link brackets",
                nested("[", "text", "]", 32768),
                Expect::Contains("text"),
            ),
            (
                "span-dense heading",
                format!("# {}", "**a** b ".repeat(1000)),
                Expect::NonEmpty,
            ),
            (
                "combined bracket/emphasis/span soup",
                "[**x**] ".repeat(65535 / 8),
                Expect::NonEmpty,
            ),
            (
                "paren flood link dest",
                "[](".repeat(21845),
                Expect::Verbatim,
            ),
            (
                "paren flood with link text",
                "[a](".repeat(16383),
                Expect::Verbatim,
            ),
            (
                "paren flood image construct",
                "![a](".repeat(13107),
                Expect::Verbatim,
            ),
            (
                "backtick flood",
                format!("a{}", "`".repeat(65534)),
                Expect::Verbatim,
            ),
            (
                "nested link-text backtick flood",
                format!("[a{}](u)", "`".repeat(65528)),
                Expect::NonEmpty,
            ),
            (
                "nested link-text paren flood",
                format!("[{}](u)", "[](".repeat(21841)),
                Expect::NonEmpty,
            ),
        ];
        for (name, input, expect) in cases {
            let md = parse(&input);
            let ok = match expect {
                Expect::Verbatim => *md.plain_text == *input,
                Expect::NonEmpty => !md.plain_text.is_empty(),
                Expect::Contains(s) => md.plain_text.contains(s),
                Expect::Len(n) => md.plain_text.len() == n,
            };
            assert!(ok, "adversarial case dropped or altered content: {name}");
        }
    }

    /// Speed guard for the O(n) backtick-run skip and the per-slice paren table:
    /// the backtick flood and both nested link-text floods must stay under a
    /// generous budget (500 ms at the default slack; O(n) costs ~0.1 ms, the
    /// quadratic path costs seconds). The budget scales with
    /// `TZE_HUD_TEST_BUDGET_SLACK` like the other timing tests.
    #[test]
    fn adversarial_floods_stay_within_budget() {
        let budget =
            std::time::Duration::from_micros(tze_hud_scene::perf_budget::test_budget(25_000));
        let floods = [
            ("backtick flood", format!("a{}", "`".repeat(65534))),
            (
                "nested link-text backtick flood",
                format!("[a{}](u)", "`".repeat(65528)),
            ),
            (
                "nested link-text paren flood",
                format!("[{}](u)", "[](".repeat(21841)),
            ),
        ];
        for (name, input) in floods {
            let t0 = std::time::Instant::now();
            let _ = parse(&input);
            let elapsed = t0.elapsed();
            assert!(elapsed < budget, "{name}: {elapsed:?} exceeds {budget:?}");
        }
    }

    fn miss_job(content: &str) -> PrimeJob {
        let tokens = Arc::new(MarkdownTokens::default());
        PrimeJob {
            key: MarkdownCache::compute_key(content, &tokens),
            content: Some(Arc::<str>::from(content)),
            tokens,
        }
    }

    fn wait_for_key(primer: &MarkdownPrimer, key: &[u8; 32], tries: u32) -> Arc<MarkdownCache> {
        for _ in 0..tries {
            let snap = primer.load();
            if snap.get_by_key(key).is_some() {
                return snap;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("primer never published an entry for the expected key");
    }

    #[test]
    fn primer_swaps_inline_carries_live_entries_and_resets() {
        let primer = MarkdownPrimer::new();
        let alpha = miss_job("**alpha**");
        let beta = miss_job("beta");
        let [alpha_key, beta_key] = [alpha.key, beta.key];
        primer.prime(vec![alpha.clone(), beta]);

        let before = primer.load();
        assert_eq!(before.len(), 2);
        let parsed = before
            .get_by_key(&alpha_key)
            .expect("inline result is visible");
        assert_eq!(parsed.plain_text.as_ref(), "alpha");
        assert!(before.get_by_key(&beta_key).is_some());

        primer.prime(vec![PrimeJob {
            content: None,
            ..alpha
        }]);
        let after = primer.load();
        assert_eq!(after.len(), 1);
        assert!(Arc::ptr_eq(
            &parsed.plain_text,
            &after.get_by_key(&alpha_key).unwrap().plain_text
        ));
        assert!(after.get_by_key(&beta_key).is_none());
        primer.reset();
        assert!(primer.is_empty());
        assert_eq!(before.len(), 2, "held reader snapshot stays intact");
    }

    #[test]
    fn primer_stale_publish_does_not_clobber_newer() {
        let cache = arc_swap::ArcSwap::from_pointee(MarkdownCache::new());
        let published = std::sync::atomic::AtomicU64::new(0);
        let tokens = MarkdownTokens::default();

        // Publish epoch 5 with entry "new".
        let mut newer = MarkdownCache::new();
        newer.prime("new", &tokens);
        let new_key = MarkdownCache::compute_key("new", &tokens);
        publish_if_newer(&cache, &published, Arc::new(newer), 5);
        assert!(cache.load().get_by_key(&new_key).is_some());

        // A late epoch-3 rebuild (containing "old") must be dropped.
        let mut older = MarkdownCache::new();
        older.prime("old", &tokens);
        let old_key = MarkdownCache::compute_key("old", &tokens);
        publish_if_newer(&cache, &published, Arc::new(older), 3);

        let snap = cache.load();
        assert!(
            snap.get_by_key(&new_key).is_some(),
            "newer snapshot must survive a stale store"
        );
        assert!(
            snap.get_by_key(&old_key).is_none(),
            "stale store must be dropped"
        );
    }

    #[test]
    fn primer_reader_never_sees_torn_state() {
        use std::sync::Barrier;

        let primer = MarkdownPrimer::new();
        let old = [miss_job("old heading"), miss_job("old body")];
        let large = "# New Scene\n\n".to_owned() + &"word **emphasis** ".repeat(600);
        assert!(large.len() > INLINE_PARSE_BYTE_THRESHOLD);
        let new = [miss_job("new heading"), miss_job(&large)];
        // Prior inline primes advance the epoch before the background parse.
        for i in 0..50 {
            primer.prime(vec![miss_job(&format!("prior scene {i}"))]);
        }
        primer.prime(old.to_vec());
        let start = Barrier::new(2);
        std::thread::scope(|threads| {
            threads.spawn(|| {
                start.wait();
                for _ in 0..500 {
                    let snapshot = primer.load();
                    let has_pair = |jobs: &[PrimeJob; 2]| {
                        jobs.iter()
                            .all(|job| snapshot.get_by_key(&job.key).is_some())
                    };
                    assert_eq!(snapshot.len(), 2);
                    assert_ne!(has_pair(&old), has_pair(&new));
                    std::thread::yield_now();
                }
            });
            start.wait();
            for i in 0..200 {
                primer.prime(if i % 2 == 0 {
                    new.to_vec()
                } else {
                    old.to_vec()
                });
            }
            primer.prime(new.to_vec());
            let after = wait_for_key(&primer, &new[0].key, 200);
            assert_eq!(after.len(), 2);
            let text = &after.get_by_key(&new[1].key).unwrap().plain_text;
            assert!(text.contains("New Scene") && text.contains("emphasis"));
        });
    }

    #[test]
    fn markdown_cache_separates_scopes_and_holds_only_live_entries() {
        let tokens = map(&[
            ("portal.transcript.link_color", "#0080ff"),
            ("color.link.text", "#ff0000"),
            ("portal.transcript.code_background", "#112233"),
            ("portal.divider.color", "#00ff00"),
        ]);
        let portal = MarkdownTokens::from_token_map_scoped(&tokens, MarkdownScope::Portal);
        let generic = MarkdownTokens::from_token_map_scoped(&tokens, MarkdownScope::Generic);
        let roles = |t: &MarkdownTokens| (t.link_color, t.code_background, t.separator_color);
        assert_eq!(
            roles(&portal),
            (
                parse_hex_color("#0080ff"),
                parse_hex_color("#112233"),
                parse_hex_color("#00ff00")
            )
        );
        assert_eq!(roles(&generic), (parse_hex_color("#ff0000"), None, None));
        assert_eq!(
            roles(&MarkdownTokens::from_token_map(&tokens)),
            roles(&portal)
        );

        let text = "[docs](https://example.com)";
        let mut cache = MarkdownCache::new();
        let keys = [&portal, &generic].map(|tokens| {
            let key = MarkdownCache::compute_key(text, tokens);
            assert!(cache.get_by_key(&key).is_none());
            let parsed = cache.prime(text, tokens).clone();
            assert_eq!(cache.prime(text, tokens), &parsed);
            assert_eq!(cache.get_by_key(&key), Some(&parsed));
            assert_eq!(cache.get(text, tokens), Some(&parsed));
            assert_ne!(key, MarkdownCache::compute_key("different text", tokens));
            key
        });
        assert_ne!(keys[0], keys[1]);
        assert_eq!(cache.len(), 2);
        for (key, tokens) in keys.iter().zip([&portal, &generic]) {
            let parsed = cache.get_by_key(key).unwrap();
            assert_eq!(
                parsed.spans.iter().find_map(|span| span.attr.color),
                tokens.link_color
            );
        }
        let large = "a".repeat(65_535);
        assert_eq!(cache.prime(&large, &portal).plain_text.as_ref(), large);
        assert_eq!(cache.len(), 3);
        cache.evict_except(&[keys[0]]);
        assert_eq!(cache.len(), 1);
        assert!(cache.get_by_key(&keys[0]).is_some());
        assert!(cache.get_by_key(&keys[1]).is_none());
        assert!(cache.get(&large, &portal).is_none());
    }

    #[test]
    fn portal_transcript_tokens_override_and_fall_back_to_generic() {
        let mut tokens = map(&[
            ("color.link.text", "#00CC44"),
            ("color.code.text", "#FF0000"),
            ("color.code.background", "#00CC44"),
            ("typography.code.family", "sans-serif"),
        ]);
        let roles = |t: MarkdownTokens| {
            (
                t.link_color,
                t.code_color,
                t.code_background,
                t.code_monospace,
            )
        };
        assert_eq!(
            roles(MarkdownTokens::from_token_map(&HashMap::new())),
            (None, None, None, true)
        );
        let green = parse_hex_color("#00CC44");
        assert_eq!(
            roles(MarkdownTokens::from_token_map(&tokens)),
            (green, parse_hex_color("#FF0000"), green, false)
        );
        tokens.extend(map(&[
            ("portal.transcript.link_color", "#0000FF"),
            ("portal.transcript.code_text", "#0000FF"),
            ("portal.transcript.code_background", "#0000FF"),
            ("portal.transcript.code_font_family", "monospace"),
        ]));
        let blue = parse_hex_color("#0000FF");
        assert_eq!(
            roles(MarkdownTokens::from_token_map(&tokens)),
            (blue, blue, blue, true)
        );
        tokens.insert("portal.transcript.code_background".into(), "#EEEEEE".into());
        assert_eq!(
            MarkdownTokens::from_token_map(&tokens).code_background,
            parse_hex_color("#EEEEEE")
        );
    }

    #[test]
    fn parse_hex_color_formats() {
        let white6 = parse_hex_color("#FFFFFF").expect("#RRGGBB");
        assert_eq!(white6.a, 1.0);

        let black3 = parse_hex_color("#000").expect("#RGB");
        assert_eq!(black3.r, 0.0);

        let semi = parse_hex_color("#FFFFFF80").expect("#RRGGBBAA");
        assert!(semi.a > 0.4 && semi.a < 0.6, "alpha ~0.5 from 0x80");
    }
}
