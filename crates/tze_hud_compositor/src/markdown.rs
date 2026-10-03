//! Phase-1 Markdown subset parser for tze_hud portal text.
//!
//! # Design contract (hud-5jbra.2)
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
//! - Links (rendered as styled non-navigable text; destination ignored)
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
//! - Autolinks (`<https://...>`)
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
    ///
    /// Cross-ref: `PORTAL_LINE_HEIGHT_MULTIPLIER` in
    /// `crates/tze_hud_projection/src/bin/projection_authority.rs` mirrors this
    /// value and must be updated in the same PR when it changes.
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
    /// turn separator (hud-nx7yq.4).  `None` disables separator rendering: a
    /// thematic-break line then renders as ordinary text (no divider), preserving
    /// pre-existing behavior for surfaces with no separator token configured.
    ///
    /// Resolved from `portal.divider.color`.  Rendered as a thin geometry quad on
    /// the break's line — content-free, so it carries no text and reveals nothing
    /// under redaction (the transcript units are zeroed upstream when redacted).
    pub separator_color: Option<Rgba>,
    /// Thickness (physical px) of the thematic-break divider quad (hud-nx7yq.4).
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

/// Which markdown surface a token set is being resolved for (hud-3ryie).
///
/// The text-stream portal transcript is a *governed* markdown surface: it may
/// restyle its own code/link/divider treatment via `portal.*`-namespaced token
/// keys (minted in hud-8691s). Any *other* markdown surface must resolve only
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
pub enum MarkdownScope {
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
    /// [`MarkdownScope`] (hud-3ryie).
    ///
    /// [`MarkdownScope::Portal`] prefers `portal.*`-namespaced keys (code/link
    /// treatment, divider) over their generic fallbacks;
    /// [`MarkdownScope::Generic`] ignores every `portal.*` key and resolves only
    /// the generic (`color.*` / `typography.*` / `spacing.*`) keys, so a
    /// portal-scoped preference can never reach a non-portal surface.
    pub fn from_token_map_scoped(map: &HashMap<String, String>, scope: MarkdownScope) -> Self {
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

        // Portal markdown-subset preference (Promotion P2, hud-8691s): for the
        // markdown-subset code/link styling the transcript renders, prefer the
        // portal-scoped canonical keys (`portal.transcript.*`) over the generic
        // ones (`color.code.*` / `color.link.text` / `typography.code.family`),
        // falling back to the generic key when the portal key is unset. This lets
        // a portal profile restyle its own code/link treatment while the generic
        // markdown defaults are preserved when no portal key is present.
        //
        // Per-tile scoping (hud-3ryie): for the portal surface, prefer the
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
        // = "monospace" | "sans-serif" | ...
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

        // Transcript turn separator: portal.divider.color / .thickness_px (hud-nx7yq.4).
        // These are `portal.`-namespaced, so they resolve for the portal scope
        // only; a non-portal (`Generic`) surface leaves the separator disabled
        // (a `---` line renders as ordinary text) (hud-3ryie).
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
        }

        tokens
    }

    /// Fold every parse-affecting styling field into `hasher` so the markdown
    /// parse cache can key on the *token-set identity* alongside the content
    /// (hud-3ryie).
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
/// fully-transparent color hash differently (hud-3ryie).
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
}

impl StyleAttr {
    /// The "no styling" identity — used for spans with no markdown decoration.
    pub fn plain() -> Self {
        Self {
            weight: None,
            italic: false,
            monospace: false,
            color: None,
            background_color: None,
            size_scale: None,
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
    /// transcript turn separators (hud-nx7yq.4).  Each offset points at the start
    /// of a blank line the renderer draws a token-styled divider quad on.
    ///
    /// Non-empty only when `tokens.separator_color` is `Some`; a thematic break
    /// (`---`/`***`/`___`) is otherwise treated as ordinary text.  Sorted
    /// ascending (parse order is source order).
    pub thematic_breaks: Vec<usize>,
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
    /// (hud-3ryie), so the same content parsed under a different token set is a
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
    /// surface) yields two distinct keys and never collides in the cache
    /// (hud-3ryie).  Two token sets with identical parse-affecting fields hash
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
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the cache is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Evict all entries whose raw content is no longer in `live_keys`.
    ///
    /// Each element of `live_keys` should be the BLAKE3 hash of a content
    /// string still referenced by the scene.  This keeps the cache bounded
    /// when scene nodes are removed.
    pub fn evict_except(&mut self, live_keys: &[[u8; 32]]) {
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
                // Parse with the job's own scoped tokens (hud-3ryie): each job
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
pub struct PrimeJob {
    /// BLAKE3 cache key (see [`MarkdownCache::compute_key`]) — folds the content
    /// **and** [`Self::tokens`]' identity, so a portal and a non-portal surface
    /// with identical content occupy distinct entries (hud-3ryie).
    pub key: [u8; 32],
    /// Raw markdown source — `Some` only when this key needs parsing.
    pub content: Option<Arc<str>>,
    /// The scoped token set this content must be parsed with (hud-3ryie).
    ///
    /// Selected per owning tile: [`MarkdownScope::Portal`] tokens for a governed
    /// portal transcript surface, [`MarkdownScope::Generic`] tokens otherwise.
    /// Held behind [`Arc`] so the two shared scope token sets are cloned by a
    /// refcount bump rather than a deep copy, and ship to the background parse
    /// thread without duplication.  `key` must have been computed with this same
    /// token set, or a cache miss will result.
    pub tokens: Arc<MarkdownTokens>,
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

/// Async, lock-free front end for the markdown parse cache (hud-33qo7).
///
/// # What this adds over [`MarkdownCache`]
///
/// hud-380dl (Option A) moved parsing **off the per-frame render path** by
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
/// dev-dependency), and the dependency bar (`about/craft-and-care`) prefers std
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
/// # hud-u4lq2: why the ordering token is NOT `SceneGraph::version`
///
/// Earlier versions of this type gated the stale-clobber guard on the
/// caller-supplied `scene.version` directly. That is broken: `SceneGraph::version`
/// is only guaranteed monotonically increasing across the lifetime of ONE
/// `SceneGraph` instance — it restarts at 0 for every newly constructed
/// `SceneGraph`. A `Compositor` (and therefore its one long-lived
/// `MarkdownPrimer`) that gets reused across multiple, independently
/// constructed `SceneGraph`s — e.g. a benchmark/test loop that builds a fresh
/// scene per scenario iteration while reusing one `Compositor`, such as
/// `scroll_reshape_bench_hud991cj` — can dispatch a legitimate prime for a NEW
/// scene whose `version` is *lower* than the value already recorded from a
/// PRIOR scene. Gating on that raw number then makes `publish_if_newer` treat
/// the new scene's correctly-parsed background result as "stale" and silently,
/// PERMANENTLY drop it: the cache never gains an entry for that content's key,
/// so every subsequent frame's `collect_text_items_from_node` lookup misses and
/// pays a full synchronous re-parse forever (the documented-as-rare inline
/// fallback in `renderer/text.rs`), matching the exact symptom reported in
/// hud-u4lq2 (confirmed via `publish_if_newer` instrumentation: the dispatch for
/// the 64KiB scenario carried `version=4` while `published_version` was already
/// `5` from the prior scenario, so `4 < 5` unconditionally dropped it).
///
/// The fix: `MarkdownPrimer` now stamps every [`Self::prime`]/[`Self::reset`]
/// dispatch with its OWN internally generated, ever-increasing epoch
/// ([`Self::next_epoch`]) instead of trusting the caller's scene-relative
/// number. An epoch depends only on call order on this primer, so it can never
/// go backwards regardless of how many different `SceneGraph` instances share
/// it over its lifetime.
pub struct MarkdownPrimer {
    /// The current cache snapshot, swapped atomically.  Readers `load()` it
    /// lock-free; the commit thread and the background worker `store()` it.
    cache: Arc<arc_swap::ArcSwap<MarkdownCache>>,
    /// Highest internal epoch for which a snapshot has been published. Guards
    /// against a late background result clobbering a newer snapshot. See the
    /// hud-u4lq2 note on [`MarkdownPrimer`] for why this is an
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
    /// scoped token set (hud-3ryie), so the rebuild needs no ambient tokens.
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
    pub fn new() -> Self {
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
    pub fn load(&self) -> Arc<MarkdownCache> {
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
    /// Every call claims a fresh internal epoch up front (see the hud-u4lq2 note
    /// on [`MarkdownPrimer`]) so the stale-clobber guard orders purely by this
    /// primer's own call sequence, never by caller-supplied scene numbering.
    pub fn prime(&self, jobs: Vec<PrimeJob>) {
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
    pub fn reset(&self) {
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

    /// Number of entries in the current snapshot (test/diagnostic use).
    #[inline]
    pub fn len(&self) -> usize {
        self.cache.load().len()
    }

    /// Whether the current snapshot is empty (test/diagnostic use).
    #[inline]
    pub fn is_empty(&self) -> bool {
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
/// [`MarkdownPrimer::next_epoch`] counter (see the hud-u4lq2 note on that
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
    /// tests can prove a code path performs ZERO markdown parses (hud-5wx09: the
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
/// and autolinks are **not** parsed.  Their literal source text is included in
/// the output verbatim so transcript content is never silently dropped.
pub fn parse_markdown_subset(content: &str, tokens: &MarkdownTokens) -> ParsedMarkdown {
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
    // Thematic-break divider offsets (transcript turn separators, hud-nx7yq.4).
    let mut thematic_breaks: Vec<usize> = Vec::new();

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
                            },
                        });
                    }
                    prev_was_empty = body_line.is_empty();
                    i += 1;
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
        // renderer draws a token-styled divider quad on (hud-nx7yq.4).
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

    ParsedMarkdown {
        plain_text: Arc::from(plain),
        spans,
        code_panels,
        list_items,
        thematic_breaks,
        line_height_multiplier: tokens.line_height_multiplier,
    }
}

/// Whether `line` is a CommonMark thematic break: three or more matching `-`,
/// `*`, or `_` characters, optionally separated by spaces (hud-nx7yq.4).
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
                        let mut link_attr = base_override.cloned().unwrap_or_else(StyleAttr::plain);
                        if tokens.link_color.is_some() {
                            link_attr.color = tokens.link_color;
                        }
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
                        if start < end {
                            fill_gaps_with_base(&link_attr, start, end, prev_len, spans);
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

    // ─── Timing-assertion gate (hud-94vm5) ───────────────────────────────────────

    /// Returns `true` when wall-clock / p99 latency hard assertions should run.
    ///
    /// Set `TZE_HUD_PERF_ASSERT=1` to enable.  On the standard `test-unit` / blocking
    /// CI lane this is unset; wall-clock budget assertions are skipped to
    /// avoid flakes from scheduler noise on shared runners.
    fn perf_assert_enabled() -> bool {
        std::env::var("TZE_HUD_PERF_ASSERT")
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
    }

    fn tokens() -> MarkdownTokens {
        MarkdownTokens::default()
    }

    fn parse(s: &str) -> ParsedMarkdown {
        parse_markdown_subset(s, &tokens())
    }

    // ─── Transcript turn separators (hud-nx7yq.4) ────────────────────────────

    fn tokens_with_separator() -> MarkdownTokens {
        MarkdownTokens {
            separator_color: Some(Rgba::new(0.16, 0.2, 0.27, 1.0)),
            ..MarkdownTokens::default()
        }
    }

    #[test]
    fn is_thematic_break_recognizes_valid_markers() {
        for s in [
            "---", "***", "___", "----", "- - -", "  ---  ", "* * *", "-- -",
        ] {
            assert!(is_thematic_break(s), "{s:?} should be a thematic break");
        }
        // Fewer than 3 markers, mixed markers, or any other character disqualify.
        for s in ["", "--", "**", "---x", "abc", "-*-", "a---", "==="] {
            assert!(
                !is_thematic_break(s),
                "{s:?} should NOT be a thematic break"
            );
        }
    }

    /// With a separator token set, a `---` line becomes a divider: recorded in
    /// `thematic_breaks`, rendered as a blank line (not literal `---` text).
    #[test]
    fn thematic_break_recorded_with_separator_token() {
        let parsed = parse_markdown_subset("A\n---\nB", &tokens_with_separator());
        assert_eq!(parsed.thematic_breaks.len(), 1, "one divider recorded");
        let plain = parsed.plain_text.as_ref();
        assert!(
            !plain.contains("---"),
            "divider is not literal text: {plain:?}"
        );
        // The recorded offset is at the divider's blank line (1 newline before it).
        let off = parsed.thematic_breaks[0];
        let lines_before = plain[..off].chars().filter(|&c| c == '\n').count();
        assert_eq!(
            lines_before, 1,
            "divider sits on the blank line between A and B"
        );
        // Both entries survive around the divider.
        assert!(plain.starts_with('A'));
        assert!(plain.ends_with('B'));
    }

    /// Without a separator token, `---` keeps the prior behavior — ordinary text,
    /// no divider recorded (minimal blast radius for non-portal surfaces).
    #[test]
    fn thematic_break_ignored_without_separator_token() {
        let parsed = parse("A\n---\nB");
        assert!(
            parsed.thematic_breaks.is_empty(),
            "no divider without the token"
        );
        assert!(
            parsed.plain_text.contains("---"),
            "without the token, --- renders as literal text"
        );
    }

    /// Multiple entries produce one divider between each adjacent pair.
    #[test]
    fn multiple_thematic_breaks_recorded_in_order() {
        let parsed = parse_markdown_subset("A\n---\nB\n---\nC", &tokens_with_separator());
        assert_eq!(parsed.thematic_breaks.len(), 2);
        assert!(
            parsed.thematic_breaks[0] < parsed.thematic_breaks[1],
            "sorted"
        );
    }

    #[test]
    fn separator_tokens_resolve_from_map() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert("portal.divider.color".to_owned(), "#2A3344".to_owned());
        map.insert("portal.divider.thickness_px".to_owned(), "2".to_owned());
        let t = MarkdownTokens::from_token_map(&map);
        assert!(t.separator_color.is_some(), "separator color resolved");
        assert_eq!(t.separator_thickness_px, 2.0);

        // Absent token → no separator, default thickness.
        let empty = MarkdownTokens::from_token_map(&HashMap::new());
        assert!(empty.separator_color.is_none());
        assert_eq!(empty.separator_thickness_px, 1.0);
    }

    // ── Task 2.4 — Subset construct tests ─────────────────────────────────────

    /// H1 heading strips `#` marker and applies bold weight.
    #[test]
    fn heading_h1_stripped_and_styled() {
        let md = parse("# Hello World");
        assert_eq!(
            &*md.plain_text, "Hello World",
            "H1 heading must strip # prefix"
        );
        assert!(
            md.spans.iter().any(|s| s.attr.weight == Some(700)
                && md.plain_text[s.start_byte..s.end_byte].contains("Hello")),
            "H1 must have weight=700 span"
        );
    }

    /// H2–H6 headings each get the correct level-specific weight from tokens.
    #[test]
    fn headings_h1_through_h6_have_correct_weights() {
        for level in 1u8..=6 {
            let marker = "#".repeat(level as usize);
            let input = format!("{marker} Text");
            let md = parse(&input);
            assert_eq!(&*md.plain_text, "Text", "level {level}: expected 'Text'");
            let expected_weight = MarkdownTokens::default().heading_weight[(level - 1) as usize];
            assert!(
                md.spans
                    .iter()
                    .any(|s| s.attr.weight == Some(expected_weight)),
                "H{level} must carry weight={expected_weight}"
            );
        }
    }

    /// Strong (`**text**`) renders as bold.
    #[test]
    fn strong_renders_bold() {
        let md = parse("Hello **world**!");
        assert_eq!(&*md.plain_text, "Hello world!");
        let bold_span = md.spans.iter().find(|s| s.attr.weight == Some(700));
        assert!(bold_span.is_some(), "strong must produce weight=700 span");
        let span = bold_span.unwrap();
        assert_eq!(&md.plain_text[span.start_byte..span.end_byte], "world");
    }

    /// Emphasis (`*text*`) renders as italic.
    #[test]
    fn emphasis_renders_italic() {
        let md = parse("Hello *world*!");
        assert_eq!(&*md.plain_text, "Hello world!");
        let italic_span = md.spans.iter().find(|s| s.attr.italic);
        assert!(italic_span.is_some(), "emphasis must produce italic span");
        let span = italic_span.unwrap();
        assert_eq!(&md.plain_text[span.start_byte..span.end_byte], "world");
    }

    /// Bold+italic (`***text***`) renders as both bold and italic.
    #[test]
    fn bold_italic_renders_both() {
        let md = parse("***bold-italic***");
        assert_eq!(&*md.plain_text, "bold-italic");
        assert!(
            md.spans
                .iter()
                .any(|s| s.attr.weight == Some(700) && s.attr.italic),
            "bold-italic must produce weight=700 and italic=true"
        );
    }

    /// Inline code (`\`code\``) uses monospace family.
    #[test]
    fn inline_code_uses_monospace() {
        let md = parse("Use `fmt::Display` here.");
        assert_eq!(&*md.plain_text, "Use fmt::Display here.");
        let code_span = md.spans.iter().find(|s| s.attr.monospace);
        assert!(
            code_span.is_some(),
            "inline code must produce monospace span"
        );
        let span = code_span.unwrap();
        assert_eq!(
            &md.plain_text[span.start_byte..span.end_byte],
            "fmt::Display"
        );
    }

    /// Fenced code block (``` ``` ```) renders body as monospace.
    #[test]
    fn fenced_code_block_body_is_monospace() {
        let input = "```\nlet x = 1;\nprintln!(\"{x}\");\n```";
        let md = parse(input);
        assert!(
            md.plain_text.contains("let x = 1;"),
            "fenced block body must be in output"
        );
        assert!(
            md.plain_text.contains("println!"),
            "fenced block body second line must be in output"
        );
        assert!(
            md.spans.iter().any(|s| s.attr.monospace),
            "fenced block must produce monospace spans"
        );
    }

    /// Indented code block (4-space indent) renders as monospace.
    #[test]
    fn indented_code_block_is_monospace() {
        let input = "    fn hello() {}";
        let md = parse(input);
        assert_eq!(&*md.plain_text, "fn hello() {}");
        assert!(
            md.spans.iter().any(|s| s.attr.monospace),
            "indented block must produce monospace span"
        );
    }

    /// Unordered list items (`- item`) render with bullet prefix.
    #[test]
    fn unordered_list_items_have_bullet() {
        let input = "- alpha\n- beta\n- gamma";
        let md = parse(input);
        assert!(
            md.plain_text.contains("• alpha"),
            "list item must have bullet; got: {:?}",
            md.plain_text
        );
        assert!(md.plain_text.contains("• beta"));
        assert!(md.plain_text.contains("• gamma"));
    }

    /// Ordered list items (`1. item`) render with their ordinal preserved.
    ///
    /// Before hud-f8jb0 the ordinal was stripped and replaced with "• ",
    /// causing "1. first" to appear as "• first".  This test codifies the
    /// correct behaviour: the ordinal number and punctuation are preserved.
    #[test]
    fn ordered_list_items_preserve_ordinal() {
        let input = "1. first\n2. second";
        let md = parse(input);
        assert!(
            md.plain_text.contains("1. first"),
            "ordered list item must preserve ordinal: got {:?}",
            md.plain_text
        );
        assert!(
            md.plain_text.contains("2. second"),
            "ordered list item must preserve ordinal: got {:?}",
            md.plain_text
        );
        // Unordered bullet must NOT appear for ordered lists.
        assert!(
            !md.plain_text.contains("• first"),
            "ordered list must not render with unordered bullet"
        );
    }

    /// Link `[text](url)` renders as styled text; destination is omitted.
    #[test]
    fn link_renders_text_not_url() {
        let md = parse("[release notes](https://example.com)");
        assert_eq!(
            &*md.plain_text, "release notes",
            "link must render only link text"
        );
        assert!(
            !md.plain_text.contains("example.com"),
            "link destination must not appear in output"
        );
    }

    /// Link text styled with token link color if present.
    #[test]
    fn link_styled_with_token_color() {
        let t = MarkdownTokens {
            link_color: Some(Rgba::new(0.0, 0.5, 1.0, 1.0)),
            ..MarkdownTokens::default()
        };
        let md = parse_markdown_subset("[click here](https://x.com)", &t);
        assert_eq!(&*md.plain_text, "click here");
        let link_span = md.spans.iter().find(|s| s.attr.color.is_some());
        assert!(
            link_span.is_some(),
            "link must carry color override when token set"
        );
    }

    // ── Task 2.4 — Excluded construct tests ───────────────────────────────────

    /// Markdown table renders as literal source text (not parsed).
    #[test]
    fn excluded_table_renders_literal() {
        let input = "| A | B |\n|---|---|\n| 1 | 2 |";
        let md = parse(input);
        // The table syntax should appear verbatim in the output.
        assert!(
            md.plain_text.contains("|"),
            "table pipes must appear literally"
        );
        assert!(
            md.plain_text.contains("A"),
            "table content must appear literally"
        );
    }

    /// Image `![alt](url)` renders as its full literal source — not dropped, not transformed.
    ///
    /// Per the excluded-construct contract: the verbatim source substring is
    /// emitted so no content is silently lost.
    #[test]
    fn excluded_image_renders_literal_source() {
        let md = parse("![diagram](img.png)");
        // The full source must appear verbatim — not silently dropped.
        assert!(
            !md.plain_text.is_empty(),
            "image construct must not be silently dropped"
        );
        assert_eq!(
            &*md.plain_text, "![diagram](img.png)",
            "image must render as verbatim source; got: {:?}",
            md.plain_text
        );
    }

    /// Raw HTML is rendered literally (not parsed or dropped).
    #[test]
    fn excluded_raw_html_renders_literally() {
        let input = "<strong>bold</strong>";
        let md = parse(input);
        // Raw HTML angle brackets should appear literally.
        assert!(
            md.plain_text.contains('<'),
            "raw HTML must not be dropped; got: {:?}",
            md.plain_text
        );
    }

    /// Blockquote (`> text`) renders as literal text (not styled as a blockquote).
    #[test]
    fn excluded_blockquote_renders_literally() {
        let input = "> This is a blockquote";
        let md = parse(input);
        assert!(
            md.plain_text.contains('>'),
            "blockquote marker must appear literally"
        );
        assert!(
            md.plain_text.contains("This is a blockquote"),
            "blockquote content must not be dropped"
        );
    }

    /// Strikethrough (`~~text~~`) renders as literal text.
    #[test]
    fn excluded_strikethrough_renders_literally() {
        let input = "~~crossed out~~";
        let md = parse(input);
        assert!(
            md.plain_text.contains("crossed out"),
            "strikethrough content must appear literally"
        );
        assert!(
            md.plain_text.contains("~~"),
            "strikethrough markers must appear literally (not parsed)"
        );
    }

    /// Task list (`- [ ] item`) renders as literal text (not a checkbox widget).
    #[test]
    fn excluded_task_list_renders_literally() {
        let input = "- [ ] todo item";
        let md = parse(input);
        // The checkbox syntax [ ] should remain visible in some form.
        // (May render as bullet item; the key property is content not dropped.)
        assert!(
            md.plain_text.contains("todo item"),
            "task list content must not be dropped"
        );
    }

    /// Link non-navigability: link text renders, no URL in output, and the
    /// span carries no href or navigation attribute.
    #[test]
    fn link_not_navigable_no_url_in_output() {
        let md = parse("[RFC 0001](https://internal.example/rfc/0001)");
        assert_eq!(&*md.plain_text, "RFC 0001");
        assert!(!md.plain_text.contains("http"), "URL must not appear");
        // No span should carry a URL (StyledSpan has no href field — this is structural).
    }

    // ── Task 2.5 — Zero per-frame parse cost ──────────────────────────────────

    /// Cache hit for the same content string is guaranteed after prime().
    #[test]
    fn cache_hit_after_prime() {
        let mut cache = MarkdownCache::new();
        let t = tokens();
        let content = "# Hello\n**world**";

        // First call primes the cache.
        let first = cache.prime(content, &t).clone();
        // Second call must hit the cache (no re-parse — same result).
        let second = cache.prime(content, &t).clone();

        assert_eq!(
            first, second,
            "cached result must be identical to parsed result"
        );
        assert_eq!(cache.len(), 1, "only one cache entry for the same content");
    }

    /// Two different content strings produce separate cache entries.
    #[test]
    fn different_content_different_cache_entries() {
        let mut cache = MarkdownCache::new();
        let t = tokens();

        cache.prime("# Hello", &t);
        cache.prime("# World", &t);

        assert_eq!(cache.len(), 2, "two distinct content strings → two entries");
    }

    /// `get` returns None for content not yet primed.
    #[test]
    fn cache_miss_before_prime() {
        let cache = MarkdownCache::new();
        assert!(
            cache.get("unparsed content", &tokens()).is_none(),
            "cache must be empty before prime"
        );
    }

    /// Large payload (65535 bytes) primes without panic.
    #[test]
    fn large_payload_65535_bytes_primes_without_panic() {
        let content = "a".repeat(65535);
        let mut cache = MarkdownCache::new();
        let _ = cache.prime(&content, &tokens());
        assert_eq!(cache.len(), 1);
    }

    /// `evict_except` removes stale entries.
    #[test]
    fn evict_removes_stale_entries() {
        let mut cache = MarkdownCache::new();
        let t = tokens();

        let content_a = "# Keep";
        let content_b = "# Evict";
        cache.prime(content_a, &t);
        cache.prime(content_b, &t);
        assert_eq!(cache.len(), 2);

        // Key folds content + token identity (hud-3ryie), so derive it via
        // compute_key rather than a raw content hash.
        let keep_key = MarkdownCache::compute_key(content_a, &t);
        cache.evict_except(&[keep_key]);

        assert_eq!(cache.len(), 1, "evict_except must remove stale entry");
        assert!(
            cache.get(content_a, &t).is_some(),
            "kept entry must remain after eviction"
        );
        assert!(
            cache.get(content_b, &t).is_none(),
            "evicted entry must be gone"
        );
    }

    // ── MarkdownTokens tests ───────────────────────────────────────────────────

    /// Empty token map produces defaults.
    #[test]
    fn empty_token_map_gives_defaults() {
        let map = HashMap::new();
        let t = MarkdownTokens::from_token_map(&map);
        let d = MarkdownTokens::default();
        assert_eq!(t.heading_weight, d.heading_weight);
        assert_eq!(t.heading_scale, d.heading_scale);
        assert!(t.link_color.is_none());
        assert!(t.code_monospace); // defaults to true
    }

    /// Token map with heading weight overrides.
    #[test]
    fn token_map_heading_weight_overrides() {
        let mut map = HashMap::new();
        map.insert("typography.heading.1.weight".to_string(), "900".to_string());
        map.insert("typography.heading.3.weight".to_string(), "500".to_string());
        let t = MarkdownTokens::from_token_map(&map);
        assert_eq!(t.heading_weight[0], 900, "H1 weight should be overridden");
        assert_eq!(t.heading_weight[2], 500, "H3 weight should be overridden");
        // Unset levels use defaults.
        assert_eq!(
            t.heading_weight[1],
            MarkdownTokens::default().heading_weight[1]
        );
    }

    /// Token map with link color override.
    #[test]
    fn token_map_link_color_override() {
        let mut map = HashMap::new();
        map.insert("color.link.text".to_string(), "#0066FF".to_string());
        let t = MarkdownTokens::from_token_map(&map);
        assert!(t.link_color.is_some(), "link color must be set from token");
        let c = t.link_color.unwrap();
        // #0066FF sRGB → r≈0, g≈0.14, b≈1.0 after gamma conversion — just
        // check that blue dominates.
        assert!(c.b > c.r, "blue must dominate for #0066FF");
    }

    // ── Portal markdown-subset token preference (Promotion P2, hud-8691s) ──────

    /// When BOTH a portal-scoped key and its generic counterpart are set, the
    /// portal key wins for the markdown-subset code/link styling the transcript
    /// renders (hud-8691s preference).
    #[test]
    fn portal_transcript_keys_win_over_generic() {
        let mut map = HashMap::new();
        // Generic (would-be) values.
        map.insert("color.link.text".to_string(), "#FF0000".to_string()); // red
        map.insert("color.code.text".to_string(), "#FF0000".to_string());
        map.insert("color.code.background".to_string(), "#FF0000".to_string());
        map.insert(
            "typography.code.family".to_string(),
            "sans-serif".to_string(),
        );
        // Portal-scoped overrides (must win).
        map.insert(
            "portal.transcript.link_color".to_string(),
            "#0000FF".to_string(),
        ); // blue
        map.insert(
            "portal.transcript.code_text".to_string(),
            "#0000FF".to_string(),
        );
        map.insert(
            "portal.transcript.code_background".to_string(),
            "#0000FF".to_string(),
        );
        map.insert(
            "portal.transcript.code_font_family".to_string(),
            "monospace".to_string(),
        );

        let t = MarkdownTokens::from_token_map(&map);
        let link = t.link_color.expect("link color set");
        assert!(
            link.b > link.r,
            "portal link_color (blue) must win over generic (red)"
        );
        let code_fg = t.code_color.expect("code fg set");
        assert!(
            code_fg.b > code_fg.r,
            "portal code_text (blue) must win over generic (red)"
        );
        let code_bg = t.code_background.expect("code bg set");
        assert!(
            code_bg.b > code_bg.r,
            "portal code_background (blue) must win over generic (red)"
        );
        assert!(
            t.code_monospace,
            "portal code_font_family=monospace must win over generic sans-serif"
        );
    }

    /// When a portal-scoped key is UNSET, the generic key is used (fallback), so
    /// nothing regresses for maps that only carry the generic keys (hud-8691s).
    #[test]
    fn portal_transcript_falls_back_to_generic_when_portal_unset() {
        let mut map = HashMap::new();
        map.insert("color.link.text".to_string(), "#00CC44".to_string()); // green
        map.insert("color.code.background".to_string(), "#00CC44".to_string());
        map.insert(
            "typography.code.family".to_string(),
            "sans-serif".to_string(),
        );
        // No portal.transcript.* keys present.

        let t = MarkdownTokens::from_token_map(&map);
        let link = t.link_color.expect("link color falls back to generic");
        assert!(
            link.g > link.r && link.g > link.b,
            "generic green link used as fallback"
        );
        assert!(
            t.code_background.is_some(),
            "generic code background used as fallback"
        );
        assert!(
            !t.code_monospace,
            "generic sans-serif code family used as fallback"
        );
    }

    /// With neither the portal nor the generic key set, the markdown-subset code/
    /// link styling stays unset (canonical default = unset), so the transcript
    /// renders exactly as before promotion (hud-8691s propagation/default).
    #[test]
    fn portal_transcript_unset_leaves_defaults() {
        let map = HashMap::new();
        let t = MarkdownTokens::from_token_map(&map);
        assert!(t.link_color.is_none(), "link unset by default");
        assert!(t.code_color.is_none(), "code fg unset by default");
        assert!(t.code_background.is_none(), "code bg unset by default");
        assert!(t.code_monospace, "code family defaults to monospace");
    }

    /// A profile swap (changing only the portal-scoped keys) reskins the resolved
    /// markdown tokens on the next resolve — the propagation the AC requires
    /// (hud-8691s). Distinct portal values yield distinct resolved colors.
    #[test]
    fn portal_transcript_profile_swap_reskins_markdown_tokens() {
        let mut profile_a = HashMap::new();
        profile_a.insert(
            "portal.transcript.code_background".to_string(),
            "#111111".to_string(),
        );
        let a = MarkdownTokens::from_token_map(&profile_a);

        let mut profile_b = HashMap::new();
        profile_b.insert(
            "portal.transcript.code_background".to_string(),
            "#EEEEEE".to_string(),
        );
        let b = MarkdownTokens::from_token_map(&profile_b);

        assert_ne!(
            a.code_background, b.code_background,
            "swapping portal.transcript.code_background must reskin the markdown tokens"
        );
    }

    /// `parse_hex_color` handles #RGB, #RRGGBB, and #RRGGBBAA.
    #[test]
    fn parse_hex_color_formats() {
        let white6 = parse_hex_color("#FFFFFF").expect("#RRGGBB");
        assert_eq!(white6.a, 1.0);

        let black3 = parse_hex_color("#000").expect("#RGB");
        assert_eq!(black3.r, 0.0);

        let semi = parse_hex_color("#FFFFFF80").expect("#RRGGBBAA");
        assert!(semi.a > 0.4 && semi.a < 0.6, "alpha ~0.5 from 0x80");
    }

    // ── Plain text and mixed content ──────────────────────────────────────────

    /// Plain text (no markdown) passes through unchanged.
    #[test]
    fn plain_text_passes_through() {
        let md = parse("Hello, world!");
        assert_eq!(&*md.plain_text, "Hello, world!");
        assert!(
            md.spans.is_empty(),
            "plain text must produce no styled spans"
        );
    }

    /// Empty input produces empty output.
    #[test]
    fn empty_input_produces_empty_output() {
        let md = parse("");
        assert_eq!(&*md.plain_text, "");
        assert!(md.spans.is_empty());
    }

    /// Multi-line content with mixed constructs covers all spans.
    #[test]
    fn mixed_content_spans_cover_plain_text() {
        let input = "# Title\n\nHello **world** and *folks*.\n\n- item one\n- item two";
        let md = parse(input);
        // Every styled span must be a valid range in plain_text.
        for span in &md.spans {
            assert!(
                span.start_byte <= span.end_byte,
                "span start must be <= end"
            );
            assert!(
                span.end_byte <= md.plain_text.len(),
                "span end must be <= plain_text.len()"
            );
            assert!(
                md.plain_text.is_char_boundary(span.start_byte),
                "span start must be a char boundary"
            );
            assert!(
                md.plain_text.is_char_boundary(span.end_byte),
                "span end must be a char boundary"
            );
        }
    }

    /// Multi-byte UTF-8 content (emoji, CJK) does not panic and produces valid boundaries.
    #[test]
    fn multibyte_utf8_content_valid_boundaries() {
        let input = "# 日本語タイトル\n\n**太字** と *斜体* テキスト。\n\n`コード`";
        let md = parse(input);
        for span in &md.spans {
            assert!(
                md.plain_text.is_char_boundary(span.start_byte),
                "start_byte must be char boundary"
            );
            assert!(
                md.plain_text.is_char_boundary(span.end_byte),
                "end_byte must be char boundary"
            );
        }
    }

    // ── Adversarial / DoS-resistance tests ────────────────────────────────────

    /// 65535 unmatched `[` characters complete in bounded time without stack
    /// overflow and without stalling the compositor.
    ///
    /// This exercises fix (b): the precomputed bracket-match table (`O(n)` single
    /// pass) replaces the original per-`[` end-of-input scan that was `O(n²)`.
    /// With 65535 brackets the old code performed ~2×10⁹ comparisons; the new
    /// code costs one `O(n)` build pass and `O(1)` lookups thereafter.
    ///
    /// The time assertion is gated by `#[ignore]` because wall-clock thresholds
    /// are not deterministic across CI runners.  Run manually with
    /// `cargo test -- --ignored` to validate timing.  Structural correctness
    /// (no panic, no dropped content) is asserted unconditionally in the
    /// `adversarial_*_no_stack_overflow` / `adversarial_*_completes_fast` tests.
    #[test]
    #[ignore = "wall-clock assertion; run with --ignored to validate timing locally"]
    fn adversarial_flood_of_unmatched_open_brackets_completes_fast() {
        let input = "[".repeat(65535);
        let deadline = std::time::Instant::now();
        let md = parse(&input);
        let elapsed = deadline.elapsed();
        // All characters must appear in the output (no content silently dropped).
        assert_eq!(
            md.plain_text.len(),
            65535,
            "all '[' must appear in plain text output"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "65535 unmatched '[' must complete in <5s (debug build); took {elapsed:?}"
        );
    }

    /// Deeply-nested bold markers (`**` × 32768 pairs) complete quickly without
    /// stack overflow.
    ///
    /// This exercises fix (a): the recursion depth cap in `process_inline_inner`.
    /// Beyond `MAX_INLINE_DEPTH` (100) the parser emits remaining characters as
    /// literals; this bounds stack consumption to a safe constant regardless of
    /// nesting depth.
    #[test]
    #[ignore = "wall-clock assertion; run with --ignored to validate timing locally"]
    fn adversarial_deeply_nested_bold_no_stack_overflow() {
        // Build "**" × 16384 + "x" + "**" × 16384 — deeply nested bold.
        let mut input = String::with_capacity(65535);
        for _ in 0..16384 {
            input.push_str("**");
        }
        input.push('x');
        for _ in 0..16384 {
            input.push_str("**");
        }
        let deadline = std::time::Instant::now();
        let md = parse(&input);
        let elapsed = deadline.elapsed();
        // Must not panic/overflow and must complete quickly.
        assert!(
            !md.plain_text.is_empty(),
            "deeply nested bold must produce non-empty output"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "deeply nested bold must complete in <5s (debug build); took {elapsed:?}"
        );
    }

    /// Deeply-nested italic markers (`*` × 32767 pairs) complete quickly without
    /// stack overflow.
    #[test]
    #[ignore = "wall-clock assertion; run with --ignored to validate timing locally"]
    fn adversarial_deeply_nested_italic_no_stack_overflow() {
        // Build "*" × 32767 + "x" + "*" × 32767
        let mut input = String::with_capacity(65535);
        for _ in 0..32767 {
            input.push('*');
        }
        input.push('x');
        for _ in 0..32767 {
            input.push('*');
        }
        let deadline = std::time::Instant::now();
        let md = parse(&input);
        let elapsed = deadline.elapsed();
        assert!(
            !md.plain_text.is_empty(),
            "deeply nested italic must produce non-empty output"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "deeply nested italic must complete in <5s (debug build); took {elapsed:?}"
        );
    }

    /// Deeply-nested link brackets (`[` × 32768 pairs) complete quickly without
    /// stack overflow.
    ///
    /// This exercises both fix (a) (depth cap) and fix (b) (bracket-match table).
    #[test]
    #[ignore = "wall-clock assertion; run with --ignored to validate timing locally"]
    fn adversarial_deeply_nested_link_brackets_no_stack_overflow() {
        // Build "[" × 32768 + "text" + "]" × 32768 — deeply nested brackets.
        let n = 32768usize;
        let mut input = String::with_capacity(n * 2 + 4);
        for _ in 0..n {
            input.push('[');
        }
        input.push_str("text");
        for _ in 0..n {
            input.push(']');
        }
        let deadline = std::time::Instant::now();
        let md = parse(&input);
        let elapsed = deadline.elapsed();
        assert!(
            md.plain_text.contains("text"),
            "link bracket flood must not drop inner text"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "deeply nested link brackets must complete in <5s (debug build); took {elapsed:?}"
        );
    }

    /// Span-dense content (many short bold spans) completes in bounded time.
    ///
    /// This exercises fix (c): `fill_gaps_with_base` now scans only the spans
    /// added during the current `process_inline_inner` call (`O(new_spans)`)
    /// rather than the full vec (`O(all_spans)`), eliminating the `O(spans²)`
    /// blowup that would otherwise occur on span-dense content.
    #[test]
    #[ignore = "wall-clock assertion; run with --ignored to validate timing locally"]
    fn adversarial_span_dense_bold_content_completes_fast() {
        // Build a heading with ~1000 alternating bold/plain segments.
        // Each "**x** " adds one styled span; fill_gaps_with_base is called once
        // per block.  Without the O(n) fix this would be O(1000²) operations.
        let segment = "**a** b ";
        let repeat = 1000;
        let body: String = segment.repeat(repeat);
        let input = format!("# {body}");
        let deadline = std::time::Instant::now();
        let md = parse(&input);
        let elapsed = deadline.elapsed();
        assert!(
            !md.plain_text.is_empty(),
            "span-dense heading must produce non-empty output"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "span-dense content must complete in <5s (debug build); took {elapsed:?}"
        );
    }

    /// Full 64 KiB adversarial payload with all three pathological patterns
    /// combined: mixed bracket floods, emphasis nesting, and span density.
    #[test]
    #[ignore = "wall-clock assertion; run with --ignored to validate timing locally"]
    fn adversarial_combined_64kib_completes_fast() {
        // 21845 repetitions of "[**x**] " ≈ 8 bytes each ≈ ~175 KiB; cap at 65535
        let segment = "[**x**] ";
        let n = 65535 / segment.len();
        let input: String = segment.repeat(n);
        let input = &input[..input.len().min(65535)];
        let deadline = std::time::Instant::now();
        let md = parse(input);
        let elapsed = deadline.elapsed();
        assert!(
            !md.plain_text.is_empty(),
            "combined adversarial input must produce non-empty output"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "combined 64KiB adversarial input must complete in <5s (debug build); took {elapsed:?}"
        );
    }

    // ── Paren-close adversarial tests (hud-xq0uo) ────────────────────────────

    /// 21845 repetitions of `[](` complete in bounded time.
    ///
    /// Before hud-xq0uo, `find_paren_close` re-scanned the full suffix for
    /// every unmatched `(`, costing O(n²) — ~969 ms in release on real hardware.
    /// The precomputed `build_paren_matches` table reduces each lookup to O(1),
    /// making the whole line O(n).
    ///
    /// This test is NOT `#[ignore]`-gated: O(n²) would make it hang for
    /// tens of seconds in any build mode; O(n) finishes instantly.
    #[test]
    fn adversarial_paren_flood_link_dest_completes_fast() {
        // "[](": 21845 repetitions ≈ 65535 bytes, all parens unmatched.
        // Since no `)(` closes any `(`, every character is emitted literally.
        let input = "[](".repeat(21845);
        let md = parse(&input);
        // Every source character must be preserved verbatim — no silent drops.
        assert_eq!(
            &*md.plain_text, input,
            "paren flood must emit all source characters verbatim"
        );
    }

    /// 16383 repetitions of `[a](` complete in bounded time.
    ///
    /// Variant: link text present (`a`) — exercises the bracket-table lookup
    /// followed by the paren-table lookup.  Empirical: ~963 ms before fix.
    ///
    /// This test is NOT `#[ignore]`-gated: see `adversarial_paren_flood_link_dest_completes_fast`.
    #[test]
    fn adversarial_paren_flood_with_link_text_completes_fast() {
        // "[a](": 16383 repetitions ≈ 65532 bytes, no closing `)`.
        // With no matching `)`, every character is emitted literally.
        let input = "[a](".repeat(16383);
        let md = parse(&input);
        // Every source character must be preserved verbatim — no silent drops.
        assert_eq!(
            &*md.plain_text, input,
            "paren flood with link text must emit all source characters verbatim"
        );
    }

    /// 13107 repetitions of `![a](` complete in bounded time.
    ///
    /// Variant: image `!` prefix — exercises `find_link_end_with_table` which
    /// also calls `find_paren_close`.  Empirical: ~642 ms before fix.
    ///
    /// This test is NOT `#[ignore]`-gated: see `adversarial_paren_flood_link_dest_completes_fast`.
    #[test]
    fn adversarial_paren_flood_image_construct_completes_fast() {
        // "![a](": 13107 repetitions ≈ 65535 bytes, no closing `)`.
        // With no matching `)`, the image construct is never completed and
        // every character is emitted literally.
        let input = "![a](".repeat(13107);
        let md = parse(&input);
        // Every source character must be preserved verbatim — no silent drops.
        assert_eq!(
            &*md.plain_text, input,
            "image paren flood must emit all source characters verbatim"
        );
    }

    // ── Backtick-close adversarial tests (hud-xq0uo / hud-t39nw) ────────────

    /// `a` + `` ` ``×65534 completes in bounded time with a real timing assertion.
    ///
    /// Before hud-xq0uo the `BacktickCloseMemo` was introduced to short-circuit
    /// failing scans, but it only covers tick_count ≤ MAX_TICK (32).  A run of
    /// 65534 adjacent backticks produces tick_count values 65534, 65533, …, 1 as
    /// the parser advances from each internal position — all values above 32
    /// bypassed the memo and fell back to O(n) scans, yielding O(n²) total.
    ///
    /// hud-t39nw fixes this by advancing `i` by `tick_count` (skipping the
    /// entire run) instead of by 1 on close-failure.  This is correct per
    /// CommonMark: a backtick string is an indivisible token; no valid code span
    /// can begin from an interior position of an unmatched run.
    ///
    /// The timing assertion here uses a 500 ms wall-clock budget, which is orders
    /// of magnitude above the O(n) cost (~0.1 ms) and well below the O(n²) cost
    /// (~9.5 s in debug / ~628 ms in release).  It will catch any regression to
    /// the quadratic path even in slow CI environments.
    ///
    /// This test is NOT `#[ignore]`-gated: O(n) finishes instantly in all build
    /// modes; O(n²) would take seconds and the timing assertion makes that visible.
    #[test]
    fn adversarial_backtick_flood_completes_fast() {
        // "a" + "`" × 65534: a single non-backtick followed by one large run of
        // 65534 adjacent backticks.  No matching closing run exists, so no code
        // span is formed and every character is emitted literally.
        let mut input = String::with_capacity(65535);
        input.push('a');
        for _ in 0..65534 {
            input.push('`');
        }
        let t0 = std::time::Instant::now();
        let md = parse(&input);
        let elapsed = t0.elapsed();
        // Structural assertion: always runs — correctness invariant, not speed.
        assert_eq!(
            &*md.plain_text, input,
            "backtick flood must emit all source characters verbatim"
        );
        // Timing assertion: gated — wall-clock budget.  (hud-94vm5)
        if perf_assert_enabled() {
            assert!(
                elapsed < std::time::Duration::from_millis(500),
                "backtick flood must complete in <500ms (O(n)); took {elapsed:?} — likely O(n²) regression"
            );
        } else {
            eprintln!(
                "[SKIP-TIMING] adversarial_backtick_flood elapsed={elapsed:?}; \
                 set TZE_HUD_PERF_ASSERT=1 to enforce 500ms budget"
            );
        }
    }

    /// `[` + `a` + `` ` ``×65528 + `](u)` completes in bounded time.
    ///
    /// The outer link `[…](u)` is valid; its inner text is `a` + `` ` ``×65528.
    /// The recursive call to `process_inline_inner` for the link text operates on
    /// the sub-slice and must not regress to O(n²) on the backtick run.
    ///
    /// Before hud-t39nw, the recursive call advanced by 1 per backtick position
    /// and all tick_counts above MAX_TICK (32) bypassed the memo, costing ~612 ms
    /// in release / ~9.5 s in debug.  After the fix the run is skipped in O(1).
    ///
    /// This test is NOT `#[ignore]`-gated: O(n) finishes instantly; O(n²) would
    /// exceed the 500 ms timing assertion even in fast CI environments.
    #[test]
    fn adversarial_nested_link_text_backtick_flood_completes_fast() {
        // "[" + "a" + "`" × 65528 + "](u)": the link text is `a` + 65528 backticks.
        // No closing backtick run exists inside the link text, so all backticks
        // are emitted literally within the link span.
        let mut input = String::with_capacity(65536);
        input.push('[');
        input.push('a');
        for _ in 0..65528 {
            input.push('`');
        }
        input.push_str("](u)");
        let t0 = std::time::Instant::now();
        let md = parse(&input);
        let elapsed = t0.elapsed();
        // Structural assertion: always runs — correctness invariant, not speed.
        assert!(
            !md.plain_text.is_empty(),
            "nested link backtick flood must produce non-empty output"
        );
        // Timing assertion: gated — wall-clock budget.  (hud-94vm5)
        if perf_assert_enabled() {
            assert!(
                elapsed < std::time::Duration::from_millis(500),
                "nested link-text backtick flood must complete in <500ms (O(n)); took {elapsed:?} — likely O(n²) regression"
            );
        } else {
            eprintln!(
                "[SKIP-TIMING] adversarial_nested_link_text_backtick_flood elapsed={elapsed:?}; \
                 set TZE_HUD_PERF_ASSERT=1 to enforce 500ms budget"
            );
        }
    }

    /// `[` + `[](` × 21841 + `](u)` completes in bounded time.
    ///
    /// The outer link `[…](u)` is valid; its inner text is `[](` × 21841 — a
    /// flood of opening parens with no closing `)`.  The recursive call for the
    /// link text receives a precomputed paren-match table (built by hud-t39nw);
    /// without it, each `(` triggers an O(n) fallback scan, costing O(n²) total.
    ///
    /// Before hud-t39nw, the recursive call passed `&[]` for `paren_matches` and
    /// relied on the O(n) depth scan for every `(`, costing ~518 ms in release.
    /// After the fix the paren table is rebuilt for the sub-slice, reducing each
    /// lookup to O(1).
    ///
    /// This test is NOT `#[ignore]`-gated: O(n) finishes instantly; O(n²) would
    /// exceed the 500 ms timing assertion.
    #[test]
    fn adversarial_nested_link_text_paren_flood_completes_fast() {
        // "[" + "[](", × 21841 + "](u)": the link text is a flood of `[](` that
        // contains 21841 unmatched opening parens.  No valid sub-links are formed
        // (the `]` of each `[]` closes the `[` of the same token, leaving `(`
        // unmatched).
        let mut input = String::with_capacity(65540);
        input.push('[');
        for _ in 0..21841 {
            input.push_str("[](");
        }
        input.push_str("](u)");
        let t0 = std::time::Instant::now();
        let md = parse(&input);
        let elapsed = t0.elapsed();
        // Structural assertion: always runs — correctness invariant, not speed.
        assert!(
            !md.plain_text.is_empty(),
            "nested link paren flood must produce non-empty output"
        );
        // Timing assertion: gated — wall-clock budget.  (hud-94vm5)
        if perf_assert_enabled() {
            assert!(
                elapsed < std::time::Duration::from_millis(500),
                "nested link-text paren flood must complete in <500ms (O(n)); took {elapsed:?} — likely O(n²) regression"
            );
        } else {
            eprintln!(
                "[SKIP-TIMING] adversarial_nested_link_text_paren_flood elapsed={elapsed:?}; \
                 set TZE_HUD_PERF_ASSERT=1 to enforce 500ms budget"
            );
        }
    }

    // ── Paren/backtick semantic correctness tests (hud-xq0uo) ────────────────

    /// Normal link `[text](url)` is correctly parsed after paren table is built.
    ///
    /// Ensures the paren-table lookup does not regress link parsing semantics.
    #[test]
    fn paren_table_link_semantic_correctness() {
        let md = parse("[hello](https://example.com)");
        assert_eq!(
            &*md.plain_text, "hello",
            "link text must be extracted, URL dropped"
        );
    }

    /// Link with nested parens in URL `[text](url(1))` is handled correctly.
    ///
    /// The paren table uses a depth-matching stack, so nested parens in the
    /// link destination match the outermost `)`.
    #[test]
    fn paren_table_nested_parens_in_url() {
        let md = parse("[doc](fn(arg))");
        // The link text should be emitted; the URL (including inner parens) is dropped.
        assert_eq!(
            &*md.plain_text, "doc",
            "nested parens in URL must not break link parsing"
        );
    }

    /// Multiple links on one line are all parsed correctly.
    #[test]
    fn paren_table_multiple_links_on_one_line() {
        let md = parse("[a](u1) and [b](u2)");
        assert_eq!(
            &*md.plain_text, "a and b",
            "multiple links must all be parsed correctly"
        );
    }

    /// Inline code spans are correctly parsed after the backtick memo is built.
    ///
    /// Ensures `BacktickCloseMemo` does not regress code-span parsing semantics.
    #[test]
    fn backtick_memo_inline_code_semantic_correctness() {
        let md = parse("Use `fmt::Display` here.");
        assert_eq!(&*md.plain_text, "Use fmt::Display here.");
        assert!(
            md.spans.iter().any(|s| s.attr.monospace),
            "inline code must produce a monospace span"
        );
        let span = md.spans.iter().find(|s| s.attr.monospace).unwrap();
        assert_eq!(
            &md.plain_text[span.start_byte..span.end_byte],
            "fmt::Display"
        );
    }

    /// Double-backtick code spans (`` ``code`` ``) are parsed correctly.
    ///
    /// Tests that tick_count=2 memo entries do not interfere with tick_count=1.
    #[test]
    fn backtick_memo_double_tick_span_correctness() {
        let md = parse("Look at ``a`b`` here.");
        assert_eq!(&*md.plain_text, "Look at a`b here.");
        assert!(
            md.spans.iter().any(|s| s.attr.monospace),
            "double-tick code span must produce a monospace span"
        );
    }

    /// Multiple code spans on one line are all parsed correctly.
    #[test]
    fn backtick_memo_multiple_spans_on_one_line() {
        let md = parse("`a` and `b` and `c`");
        assert_eq!(&*md.plain_text, "a and b and c");
        assert_eq!(
            md.spans.iter().filter(|s| s.attr.monospace).count(),
            3,
            "three separate code spans must each produce a monospace span"
        );
    }

    // ── hud-f8jb0: styling-gap fixes ─────────────────────────────────────────
    //
    // (a) heading_scale is now applied via size_scale on StyleAttr/StyledSpan.
    // (b) color.code.background is now read from tokens and applied.
    // (c) bold weight comes from tokens.bold_weight, not a hardcoded 700.
    // (d) ordered-list ordinal is preserved, not replaced with "• ".

    /// (a) Heading scale: H1 span carries size_scale = 1.75 (default token).
    #[test]
    fn heading_h1_size_scale_applied() {
        let md = parse("# Hello");
        let scale_span = md.spans.iter().find(|s| s.attr.size_scale.is_some());
        assert!(
            scale_span.is_some(),
            "H1 must produce a span with size_scale set; got spans: {:?}",
            md.spans
        );
        let scale = scale_span.unwrap().attr.size_scale.unwrap();
        let expected = MarkdownTokens::default().heading_scale[0];
        assert!(
            (scale - expected).abs() < 1e-5,
            "H1 size_scale must equal token heading_scale[0] ({expected}); got {scale}"
        );
    }

    /// (a) Heading scale: H3 span carries size_scale = 1.25 (default token).
    #[test]
    fn heading_h3_size_scale_applied() {
        let md = parse("### Section");
        let scale_span = md.spans.iter().find(|s| s.attr.size_scale.is_some());
        assert!(
            scale_span.is_some(),
            "H3 must produce a span with size_scale set"
        );
        let scale = scale_span.unwrap().attr.size_scale.unwrap();
        let expected = MarkdownTokens::default().heading_scale[2]; // index 2 = H3
        assert!(
            (scale - expected).abs() < 1e-5,
            "H3 size_scale must equal token heading_scale[2] ({expected}); got {scale}"
        );
    }

    /// (a) Heading scale: H5 uses scale 1.0 — size_scale is None (no scaling needed).
    ///
    /// When the scale equals 1.0 exactly the field is set to None to avoid a
    /// no-op Metrics override in the renderer.
    #[test]
    fn heading_h5_scale_one_is_none() {
        let t = MarkdownTokens {
            heading_scale: [1.75, 1.50, 1.25, 1.10, 1.00, 0.90],
            ..MarkdownTokens::default()
        };
        let md = parse_markdown_subset("##### H5", &t);
        // Default H5 scale is 1.00: None expected (no-op suppressed).
        assert!(
            md.spans.iter().all(|s| s.attr.size_scale.is_none()
                || (s.attr.size_scale.unwrap() - 1.0).abs() > f32::EPSILON),
            "H5 with scale 1.0 must not emit size_scale=Some(1.0); spans: {:?}",
            md.spans
        );
    }

    /// (a) Heading scale from custom token map flows through.
    #[test]
    fn heading_scale_from_custom_token_map() {
        let mut map = HashMap::new();
        map.insert("typography.heading.2.scale".to_string(), "2.0".to_string());
        let t = MarkdownTokens::from_token_map(&map);
        let md = parse_markdown_subset("## Big", &t);
        let scale_span = md.spans.iter().find(|s| s.attr.size_scale.is_some());
        assert!(
            scale_span.is_some(),
            "H2 with custom scale must carry size_scale"
        );
        let scale = scale_span.unwrap().attr.size_scale.unwrap();
        assert!(
            (scale - 2.0).abs() < 1e-5,
            "H2 size_scale must equal custom token value 2.0; got {scale}"
        );
    }

    /// (b) code_background token populates background_color, NOT the text foreground color.
    ///
    /// Previously the bug caused code_background to bleed into `StyleAttr::color` (the
    /// text foreground) when no code_color was set.  The correct behavior is:
    /// - `background_color` carries the backdrop panel color
    /// - `color` (text foreground) stays `None` when code_color is unset
    #[test]
    fn code_background_token_applied_to_inline_code() {
        let t = MarkdownTokens {
            code_background: Some(Rgba::new(0.1, 0.1, 0.1, 1.0)),
            code_color: None,
            ..MarkdownTokens::default()
        };
        let md = parse_markdown_subset("Use `fmt` here.", &t);
        let code_span = md.spans.iter().find(|s| s.attr.monospace);
        assert!(
            code_span.is_some(),
            "inline code must produce a monospace span"
        );
        let span = code_span.unwrap();
        // code_background must NOT bleed into the text foreground color.
        assert!(
            span.attr.color.is_none(),
            "code span text color must be None when only code_background is set (not code_color); \
             got {:?} — code_background is a backdrop panel, not a text color modifier",
            span.attr.color
        );
        // code_background must populate background_color instead.
        let bg = span.attr.background_color;
        assert!(
            bg.is_some(),
            "code span background_color must be Some when code_background token is set; got None"
        );
        let c = bg.unwrap();
        assert!(
            (c.r - 0.1_f32).abs() < 1e-3,
            "code span background_color.r must match code_background.r; got {c:?}"
        );
        // A CodePanelSpan of kind Inline must also be recorded.
        assert!(
            md.code_panels
                .iter()
                .any(|p| matches!(p.kind, CodePanelKind::Inline)),
            "code_panels must contain an Inline entry for the inline code span"
        );
    }

    /// (b) code_color takes priority over code_background for the text foreground;
    /// code_background still goes to background_color regardless.
    #[test]
    fn code_color_takes_priority_over_code_background() {
        let t = MarkdownTokens {
            code_color: Some(Rgba::new(0.9, 0.9, 0.9, 1.0)),
            code_background: Some(Rgba::new(0.1, 0.1, 0.1, 1.0)),
            ..MarkdownTokens::default()
        };
        let md = parse_markdown_subset("Use `fmt` here.", &t);
        let code_span = md.spans.iter().find(|s| s.attr.monospace);
        assert!(
            code_span.is_some(),
            "inline code must produce a monospace span"
        );
        let span = code_span.unwrap();
        // code_color (0.9) must be the text foreground.
        let c = span.attr.color.unwrap();
        assert!(
            (c.r - 0.9_f32).abs() < 1e-3,
            "code_color must be the text foreground; got color r={} (expected ~0.9)",
            c.r
        );
        // code_background (0.1) must be the backdrop panel, not blended into text color.
        let bg = span.attr.background_color.unwrap();
        assert!(
            (bg.r - 0.1_f32).abs() < 1e-3,
            "code_background must be background_color; got bg.r={} (expected ~0.1)",
            bg.r
        );
    }

    /// (b) color.code.background token key is parsed by from_token_map.
    #[test]
    fn token_map_code_background_parsed() {
        let mut map = HashMap::new();
        map.insert("color.code.background".to_string(), "#333333".to_string());
        let t = MarkdownTokens::from_token_map(&map);
        assert!(
            t.code_background.is_some(),
            "color.code.background token must populate code_background"
        );
    }

    /// (b) code_panel spans are emitted for fenced code blocks.
    #[test]
    fn code_panel_spans_emitted_for_fenced_block() {
        let t = MarkdownTokens {
            code_background: Some(Rgba::new(0.05, 0.05, 0.15, 1.0)),
            ..MarkdownTokens::default()
        };
        let md = parse_markdown_subset("```\nfn foo() {}\n```\n", &t);
        let block_panel = md
            .code_panels
            .iter()
            .find(|p| matches!(p.kind, CodePanelKind::Block));
        assert!(
            block_panel.is_some(),
            "code_panels must contain a Block entry for a fenced code block; got: {:?}",
            md.code_panels
        );
        // The panel must cover at least the fenced content bytes.
        let panel = block_panel.unwrap();
        assert!(
            panel.start_byte < panel.end_byte,
            "Block panel must span a non-empty byte range; got [{}, {}]",
            panel.start_byte,
            panel.end_byte
        );
    }

    /// (b) code_panel spans are emitted for inline code spans.
    #[test]
    fn code_panel_spans_emitted_for_inline_code() {
        let t = MarkdownTokens {
            code_background: Some(Rgba::new(0.05, 0.05, 0.15, 1.0)),
            ..MarkdownTokens::default()
        };
        let md = parse_markdown_subset("Use `fmt` and `clippy`.", &t);
        let inline_panels: Vec<_> = md
            .code_panels
            .iter()
            .filter(|p| matches!(p.kind, CodePanelKind::Inline))
            .collect();
        assert_eq!(
            inline_panels.len(),
            2,
            "two inline code spans must produce two Inline code_panels; got: {:?}",
            md.code_panels
        );
    }

    /// (b) No code_panels emitted when code_background token is absent.
    #[test]
    fn no_code_panels_without_code_background_token() {
        let t = MarkdownTokens {
            code_background: None,
            code_color: Some(Rgba::new(0.8, 0.8, 0.8, 1.0)),
            ..MarkdownTokens::default()
        };
        let md = parse_markdown_subset("Use `fmt` here.\n```\ncode\n```\n", &t);
        assert!(
            md.code_panels.is_empty(),
            "code_panels must be empty when code_background token is None; got: {:?}",
            md.code_panels
        );
    }

    /// (c) Bold weight comes from tokens (typography.bold.weight), not hardcoded 700.
    #[test]
    fn bold_weight_comes_from_token() {
        let t = MarkdownTokens {
            bold_weight: 800,
            ..MarkdownTokens::default()
        };
        let md = parse_markdown_subset("Hello **world**!", &t);
        let bold_span = md.spans.iter().find(|s| s.attr.weight == Some(800));
        assert!(
            bold_span.is_some(),
            "bold span must carry weight from token (800); got spans: {:?}",
            md.spans
        );
        // Must NOT carry the old hardcoded 700 when token says 800.
        assert!(
            !md.spans.iter().any(|s| s.attr.weight == Some(700)),
            "bold span must use token weight 800, not hardcoded 700"
        );
    }

    /// (c) Bold-italic weight also comes from tokens.
    #[test]
    fn bold_italic_weight_comes_from_token() {
        let t = MarkdownTokens {
            bold_weight: 900,
            ..MarkdownTokens::default()
        };
        let md = parse_markdown_subset("***bold-italic***", &t);
        let bi_span = md
            .spans
            .iter()
            .find(|s| s.attr.italic && s.attr.weight == Some(900));
        assert!(
            bi_span.is_some(),
            "bold-italic span must carry weight from token (900); got spans: {:?}",
            md.spans
        );
    }

    /// (c) typography.bold.weight token key is parsed by from_token_map.
    #[test]
    fn token_map_bold_weight_parsed() {
        let mut map = HashMap::new();
        map.insert("typography.bold.weight".to_string(), "800".to_string());
        let t = MarkdownTokens::from_token_map(&map);
        assert_eq!(
            t.bold_weight, 800,
            "typography.bold.weight token must populate bold_weight"
        );
    }

    /// (d) Ordered-list paren-delimited items preserve ordinal.
    #[test]
    fn ordered_list_paren_delimiter_preserves_ordinal() {
        let input = "1) alpha\n2) beta";
        let md = parse(input);
        assert!(
            md.plain_text.contains("1) alpha"),
            "paren-delimited ordered list must preserve ordinal; got: {:?}",
            md.plain_text
        );
        assert!(
            md.plain_text.contains("2) beta"),
            "second paren-delimited item must preserve ordinal; got: {:?}",
            md.plain_text
        );
    }

    /// (d) Unordered lists still render with bullet (regression guard).
    #[test]
    fn unordered_list_still_uses_bullet_after_fix() {
        let input = "- item one\n* item two\n+ item three";
        let md = parse(input);
        assert!(
            md.plain_text.contains("• item one"),
            "unordered '-' list must keep bullet; got: {:?}",
            md.plain_text
        );
        assert!(
            md.plain_text.contains("• item two"),
            "unordered '*' list must keep bullet; got: {:?}",
            md.plain_text
        );
        assert!(
            md.plain_text.contains("• item three"),
            "unordered '+' list must keep bullet; got: {:?}",
            md.plain_text
        );
    }

    // ── Block-spacing token tests (hud-bq0gl) ─────────────────────────────────

    /// `line_height_multiplier` token parses correctly from token map.
    #[test]
    fn token_line_height_multiplier_parses() {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "typography.line_height.multiplier".to_string(),
            "1.8".to_string(),
        );
        let tokens = MarkdownTokens::from_token_map(&map);
        assert!(
            (tokens.line_height_multiplier - 1.8).abs() < 0.001,
            "line_height_multiplier must parse from token map; got {}",
            tokens.line_height_multiplier
        );
    }

    /// `line_height_multiplier` clamps to [1.0, 4.0].
    #[test]
    fn token_line_height_multiplier_clamped() {
        let mut map_low = std::collections::HashMap::new();
        map_low.insert(
            "typography.line_height.multiplier".to_string(),
            "0.5".to_string(),
        );
        let tokens_low = MarkdownTokens::from_token_map(&map_low);
        assert_eq!(
            tokens_low.line_height_multiplier, 1.4,
            "line_height_multiplier must ignore out-of-bounds low value and remain default 1.4"
        );

        let mut map_high = std::collections::HashMap::new();
        map_high.insert(
            "typography.line_height.multiplier".to_string(),
            "10.0".to_string(),
        );
        let tokens_high = MarkdownTokens::from_token_map(&map_high);
        assert_eq!(
            tokens_high.line_height_multiplier, 1.4,
            "line_height_multiplier must ignore out-of-bounds high value and remain default 1.4"
        );
    }

    /// `heading_margin_top` and `heading_margin_bottom` tokens parse correctly.
    #[test]
    fn token_heading_margins_parse() {
        let mut map = std::collections::HashMap::new();
        map.insert("spacing.heading.top".to_string(), "2.0".to_string());
        map.insert("spacing.heading.bottom".to_string(), "1.5".to_string());
        let tokens = MarkdownTokens::from_token_map(&map);
        assert!(
            (tokens.heading_margin_top - 2.0).abs() < 0.001,
            "heading_margin_top must parse; got {}",
            tokens.heading_margin_top
        );
        assert!(
            (tokens.heading_margin_bottom - 1.5).abs() < 0.001,
            "heading_margin_bottom must parse; got {}",
            tokens.heading_margin_bottom
        );
    }

    /// `list_item_spacing` token parses correctly.
    #[test]
    fn token_list_item_spacing_parses() {
        let mut map = std::collections::HashMap::new();
        map.insert("spacing.list.item".to_string(), "1.0".to_string());
        let tokens = MarkdownTokens::from_token_map(&map);
        assert!(
            (tokens.list_item_spacing - 1.0).abs() < 0.001,
            "list_item_spacing must parse; got {}",
            tokens.list_item_spacing
        );
    }

    /// Default tokens have expected default values.
    #[test]
    fn token_defaults_are_canonical() {
        let tokens = MarkdownTokens::default();
        assert!(
            (tokens.line_height_multiplier - 1.4).abs() < 0.001,
            "default line_height_multiplier must be 1.4; got {}",
            tokens.line_height_multiplier
        );
        assert!(
            tokens.heading_margin_top.abs() < 0.001,
            "default heading_margin_top must be 0.0; got {}",
            tokens.heading_margin_top
        );
        assert!(
            tokens.heading_margin_bottom.abs() < 0.001,
            "default heading_margin_bottom must be 0.0; got {}",
            tokens.heading_margin_bottom
        );
        assert!(
            (tokens.list_item_spacing - 0.0).abs() < 0.001,
            "default list_item_spacing must be 0.0; got {}",
            tokens.list_item_spacing
        );
    }

    #[test]
    fn default_markdown_block_spacing_is_compact_for_streamed_transcripts() {
        let tokens = MarkdownTokens::default();
        assert!(
            tokens.heading_margin_top.abs() < 0.001,
            "streamed transcript headings should not insert an extra blank line by default; got top margin {}",
            tokens.heading_margin_top
        );
        assert!(
            tokens.heading_margin_bottom.abs() < 0.001,
            "streamed transcript headings should not insert an extra blank line below by default; got bottom margin {}",
            tokens.heading_margin_bottom
        );

        let md = parse_markdown_subset("Intro\n# Heading\n- first\n- second", &tokens);
        assert_eq!(
            md.plain_text.as_ref(),
            "Intro\nHeading\n• first\n• second",
            "default portal markdown should keep transcript block flow compact"
        );
    }

    /// `parse_markdown_subset` propagates `line_height_multiplier` into
    /// `ParsedMarkdown.line_height_multiplier`.
    #[test]
    fn parsed_markdown_carries_line_height_multiplier() {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "typography.line_height.multiplier".to_string(),
            "1.6".to_string(),
        );
        let tokens = MarkdownTokens::from_token_map(&map);
        let md = parse_markdown_subset("hello world", &tokens);
        assert!(
            (md.line_height_multiplier - 1.6).abs() < 0.001,
            "ParsedMarkdown must carry line_height_multiplier from tokens; got {}",
            md.line_height_multiplier
        );
    }

    /// List items produce `ListItemSpan` entries with correct `content_start_byte`
    /// greater than `item_start_byte` (hanging-indent metadata is non-trivial).
    #[test]
    fn list_items_produce_hanging_indent_metadata() {
        let input = "- first item\n- second item";
        let md = parse(input);
        assert!(
            !md.list_items.is_empty(),
            "unordered list must produce ListItemSpan entries; got none. plain_text: {:?}",
            md.plain_text
        );
        for span in &md.list_items {
            assert!(
                span.content_start_byte > span.item_start_byte,
                "content_start_byte ({}) must be > item_start_byte ({}) \
                 to encode a non-zero bullet prefix for hanging indent",
                span.content_start_byte,
                span.item_start_byte
            );
        }
    }

    /// Ordered list items also produce `ListItemSpan` entries.
    #[test]
    fn ordered_list_items_produce_hanging_indent_metadata() {
        let input = "1. alpha\n2. beta\n3. gamma";
        let md = parse(input);
        assert_eq!(
            md.list_items.len(),
            3,
            "three ordered list items must produce three ListItemSpan entries; \
             got {}. plain_text: {:?}",
            md.list_items.len(),
            md.plain_text
        );
    }

    /// With `heading_margin_top >= 1.0`, a heading preceded by content gets an
    /// extra blank line before it in `plain_text`.
    #[test]
    fn heading_top_margin_emits_blank_line_before_heading() {
        let tokens = MarkdownTokens {
            heading_margin_top: 1.0,
            ..MarkdownTokens::default()
        };
        let input = "Some text\n# Heading";
        let md = parse_markdown_subset(input, &tokens);
        assert_eq!(
            md.plain_text.as_ref(),
            "Some text\n\nHeading",
            "heading with explicit top margin must be preceded by extra whitespace",
        );
    }

    // ── MarkdownPrimer — async swap-in (hud-33qo7) ────────────────────────────

    /// Build a `PrimeJob` for `content`, marking it as a cache miss (content
    /// attached) so the primer parses it.
    fn miss_job(content: &str) -> PrimeJob {
        let tokens = Arc::new(MarkdownTokens::default());
        PrimeJob {
            key: MarkdownCache::compute_key(content, &tokens),
            content: Some(Arc::<str>::from(content)),
            tokens,
        }
    }

    /// Block until the primer's snapshot contains `key`, or fail after `tries`
    /// short sleeps.  The background parse thread publishes asynchronously, so a
    /// test that needs the *completed* swap must poll rather than read once.
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

    /// A small payload is parsed inline and is visible in the snapshot
    /// immediately after `prime` returns (no thread hop, no torn read).
    #[test]
    fn primer_small_payload_swaps_in_synchronously() {
        let primer = MarkdownPrimer::new();
        let job = miss_job("**bold** text");
        let key = job.key;

        primer.prime(vec![job]);

        let snap = primer.load();
        let parsed = snap
            .get_by_key(&key)
            .expect("small payload must be present synchronously after prime");
        assert!(parsed.plain_text.contains("bold"));
    }

    /// A large payload (above the inline threshold) is parsed on the background
    /// thread and swapped in atomically; the result is eventually visible and
    /// correct.
    #[test]
    fn primer_large_payload_swaps_in_off_thread() {
        let primer = MarkdownPrimer::new();
        // Exceed INLINE_PARSE_BYTE_THRESHOLD so the background path is taken.
        let big = "# Title\n\n".to_string() + &"word **emphasis** ".repeat(2000);
        assert!(big.len() > INLINE_PARSE_BYTE_THRESHOLD);
        let job = miss_job(&big);
        let key = job.key;

        primer.prime(vec![job]);

        // The swap completes asynchronously; poll for it.
        let snap = wait_for_key(&primer, &key, 200);
        let parsed = snap
            .get_by_key(&key)
            .expect("large payload eventually present");
        assert!(parsed.plain_text.contains("Title"));
        assert!(parsed.plain_text.contains("emphasis"));
    }

    /// Removing a node from the live set evicts its entry on the next prime
    /// (the new snapshot is exactly the live set).
    #[test]
    fn primer_evicts_dead_entries_on_reprime() {
        let primer = MarkdownPrimer::new();
        let job_a = miss_job("alpha");
        let job_b = miss_job("beta");
        let key_a = job_a.key;
        let key_b = job_b.key;

        primer.prime(vec![job_a.clone(), job_b]);
        let snap = primer.load();
        assert!(snap.get_by_key(&key_a).is_some());
        assert!(snap.get_by_key(&key_b).is_some());
        assert_eq!(snap.len(), 2);

        // Re-prime with only A live: B must be evicted, A carried forward.
        // A is already cached, so pass `content: None` (carry-forward path).
        let carry_a = PrimeJob {
            key: key_a,
            content: None,
            tokens: Arc::new(MarkdownTokens::default()),
        };
        primer.prime(vec![carry_a]);
        let snap = primer.load();
        assert!(
            snap.get_by_key(&key_a).is_some(),
            "live entry carried forward"
        );
        assert!(snap.get_by_key(&key_b).is_none(), "dead entry evicted");
        assert_eq!(snap.len(), 1);
    }

    /// `reset` empties the snapshot so token-map changes never serve stale
    /// styling.
    #[test]
    fn primer_reset_clears_snapshot() {
        let primer = MarkdownPrimer::new();
        let job = miss_job("# Heading");
        primer.prime(vec![job]);
        assert!(!primer.is_empty());

        primer.reset();
        assert!(primer.is_empty(), "reset must clear the snapshot");
    }

    /// hud-u4lq2 regression (primer-level): a `MarkdownPrimer` that has already
    /// serviced many unrelated `prime()` dispatches (modeling a `Compositor`
    /// reused across many prior scenes/content generations) must still land a
    /// background-parsed snapshot for a brand-new large payload.
    ///
    /// Before the fix, `MarkdownPrimer::prime` trusted a caller-supplied
    /// `scene_version: u64` as its stale-clobber ordering token
    /// (`published_version`). That token is only meaningful if the caller's
    /// numbering is globally monotonic across the primer's ENTIRE lifetime —
    /// but callers legitimately reset their own numbering per `SceneGraph`
    /// instance (`version` restarts at 0 for every new scene). A `Compositor`
    /// reused across scenes (confirmed in `scroll_reshape_bench_hud991cj`)
    /// could therefore dispatch a later, LOWER-numbered prime that
    /// `publish_if_newer` would treat as stale and silently, permanently drop
    /// — even though it was the most recent, correct dispatch.
    ///
    /// `prime()` no longer accepts a caller-supplied version at all (the API
    /// changed as part of the fix): every dispatch claims a fresh internal
    /// epoch from `MarkdownPrimer::next_epoch`, which can only ever increase.
    /// This test proves that guarantee holds even after many prior dispatches.
    #[test]
    fn primer_lands_large_payload_after_many_prior_dispatches() {
        let primer = MarkdownPrimer::new();

        // Model many prior "scenes" worth of small (inline-path) dispatches —
        // each one advances the primer's internal epoch, exactly as repeated
        // commit-time primes would in a long-lived Compositor session.
        for i in 0..50u32 {
            primer.prime(vec![miss_job(&format!("prior scene {i}"))]);
        }

        // A brand-new large payload (background-thread path) must still land,
        // not be dropped as "stale" relative to the prior dispatches.
        let big = "# New Scene\n\n".to_string() + &"word **emphasis** ".repeat(2000);
        assert!(big.len() > INLINE_PARSE_BYTE_THRESHOLD);
        let job = miss_job(&big);
        let key = job.key;
        primer.prime(vec![job]);

        let snap = wait_for_key(&primer, &key, 200);
        let parsed = snap.get_by_key(&key).expect(
            "large payload must land even after many prior prime() dispatches \
                 -- hud-u4lq2 regression",
        );
        assert!(parsed.plain_text.contains("New Scene"));
        assert!(parsed.plain_text.contains("emphasis"));
    }

    /// A stale background result for an older epoch must not clobber a newer
    /// snapshot already published.  We exercise the guard directly via
    /// `publish_if_newer`.
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

    /// Concurrent readers loading during an in-flight prime always observe a
    /// complete snapshot (old-or-new), never a torn map.
    #[test]
    fn primer_reader_never_sees_torn_state() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let primer = Arc::new(MarkdownPrimer::new());

        // Seed an initial complete snapshot.
        let seed = miss_job("seed content");
        let seed_key = seed.key;
        primer.prime(vec![seed]);

        let stop = Arc::new(AtomicBool::new(false));
        let reader_primer = Arc::clone(&primer);
        let reader_stop = Arc::clone(&stop);
        let reader = std::thread::spawn(move || {
            // Every snapshot the reader sees must be internally consistent:
            // either it has the seed key (old) or it has the new keys (new),
            // and its len matches its contents — never partially filled.
            while !reader_stop.load(Ordering::Relaxed) {
                let snap = reader_primer.load();
                // A snapshot is valid as long as len() == number of present
                // entries it reports; get_by_key on a present key returns a
                // fully-formed ParsedMarkdown (Arc<str> non-dangling).
                if let Some(p) = snap.get_by_key(&seed_key) {
                    assert!(!p.plain_text.is_empty());
                }
            }
        });

        // Hammer the primer with alternating live sets. Each `prime()` call
        // claims a fresh, strictly-increasing internal epoch on its own
        // (hud-u4lq2), so no caller-supplied version is needed here.
        for v in 2..200u64 {
            let content = format!("# Doc {v}\n\n{}", "body ".repeat(1000));
            primer.prime(vec![miss_job(&content)]);
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();
    }

    // ── Per-tile markdown token scoping (hud-3ryie) ───────────────────────────

    /// A map that sets BOTH a portal-scoped and a generic key for link color,
    /// plus a portal-only code background (no generic fallback), so the two
    /// scopes resolve provably different token sets.
    fn scoped_token_map() -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        // Both keys present: portal prefers its own; generic must use the
        // generic fallback and never the portal key.
        map.insert(
            "portal.transcript.link_color".to_string(),
            "#0080ff".to_string(),
        );
        map.insert("color.link.text".to_string(), "#ff0000".to_string());
        // Portal-only key with NO generic fallback: generic must resolve None.
        map.insert(
            "portal.transcript.code_background".to_string(),
            "#112233".to_string(),
        );
        // Portal-only divider namespace: generic must ignore it entirely.
        map.insert("portal.divider.color".to_string(), "#00ff00".to_string());
        map
    }

    /// (b) A non-portal (`Generic`) surface must NOT receive `portal.transcript.*`
    /// (or any `portal.*`) preference, while the portal surface does (hud-3ryie).
    #[test]
    fn generic_scope_never_receives_portal_preferences() {
        let map = scoped_token_map();
        let portal = MarkdownTokens::from_token_map_scoped(&map, MarkdownScope::Portal);
        let generic = MarkdownTokens::from_token_map_scoped(&map, MarkdownScope::Generic);

        // Link color: portal prefers the portal key; generic uses the generic
        // fallback — the two must differ, proving no portal leak.
        assert_eq!(portal.link_color, parse_hex_color("#0080ff"));
        assert_eq!(
            generic.link_color,
            parse_hex_color("#ff0000"),
            "generic scope must resolve the generic key, never the portal key"
        );
        assert_ne!(portal.link_color, generic.link_color);

        // Code background is a portal-only key here (no generic fallback set):
        // portal resolves it, generic must leave it None.
        assert_eq!(portal.code_background, parse_hex_color("#112233"));
        assert_eq!(
            generic.code_background, None,
            "a portal-only key must never reach a non-portal surface"
        );

        // Divider is portal-namespaced: portal enables it, generic disables it.
        assert_eq!(portal.separator_color, parse_hex_color("#00ff00"));
        assert_eq!(
            generic.separator_color, None,
            "portal.divider.* must never reach a non-portal surface"
        );

        // The default portal wrapper stays identical to the explicit Portal scope
        // (v1 behavior unchanged, hud-3ryie).
        let default_portal = MarkdownTokens::from_token_map(&map);
        assert_eq!(default_portal.link_color, portal.link_color);
        assert_eq!(default_portal.code_background, portal.code_background);
        assert_eq!(default_portal.separator_color, portal.separator_color);
    }

    /// (a) The parse cache must not mis-serve entries across differing token
    /// sets: the SAME content under two different token sets yields two distinct
    /// cached parses keyed independently (no collision) (hud-3ryie).
    #[test]
    fn same_content_two_token_sets_do_not_collide_in_cache() {
        let map = scoped_token_map();
        let portal = MarkdownTokens::from_token_map_scoped(&map, MarkdownScope::Portal);
        let generic = MarkdownTokens::from_token_map_scoped(&map, MarkdownScope::Generic);
        // The two scopes genuinely differ, so their parses must differ too.
        assert_ne!(portal.link_color, generic.link_color);

        let content = "[docs](https://example.com)";

        // Distinct keys for the same content under the two token sets.
        let key_portal = MarkdownCache::compute_key(content, &portal);
        let key_generic = MarkdownCache::compute_key(content, &generic);
        assert_ne!(
            key_portal, key_generic,
            "content + token identity must produce distinct keys per token set"
        );

        // Priming the same content under each token set produces TWO entries,
        // each serving its own parse — never one clobbering the other.
        let mut cache = MarkdownCache::new();
        let parsed_portal = cache.prime(content, &portal).clone();
        let parsed_generic = cache.prime(content, &generic).clone();
        assert_eq!(
            cache.len(),
            2,
            "same content under two token sets must occupy two distinct entries"
        );

        // Each key resolves to the parse produced under its own token set.
        assert_eq!(cache.get_by_key(&key_portal), Some(&parsed_portal));
        assert_eq!(cache.get_by_key(&key_generic), Some(&parsed_generic));

        // And the parses themselves differ: the portal link carries a color
        // override span; the generic one (no generic link color here... actually
        // generic sets #ff0000) also colors, but with a DIFFERENT color — so the
        // parsed outputs are not equal.
        assert_ne!(
            parsed_portal, parsed_generic,
            "the two token sets must yield materially different parses"
        );
        let portal_link_color = parsed_portal
            .spans
            .iter()
            .find_map(|s| s.attr.color)
            .expect("portal link span must carry a color");
        let generic_link_color = parsed_generic
            .spans
            .iter()
            .find_map(|s| s.attr.color)
            .expect("generic link span must carry a color");
        assert_ne!(
            portal_link_color, generic_link_color,
            "portal vs generic link color must differ (no cross-scope mis-serve)"
        );
    }
}
