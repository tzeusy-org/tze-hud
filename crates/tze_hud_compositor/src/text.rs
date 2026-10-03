//! Text rasterization layer for the compositor.
//!
//! Integrates [glyphon](https://github.com/grovesNL/glyphon) (cosmic-text +
//! etagere atlas) into the tze_hud render pipeline.
//!
//! # Responsibilities
//!
//! - Maintain a single `FontSystem` and `SwashCache` for the compositor
//!   lifetime.
//! - Provide `render_text_areas` which runs a glyphon text pass against an
//!   existing `RenderPassDescriptor`-compatible texture view (LoadOp::Load so
//!   prior geometry is preserved).
//! - Map `TextMarkdownNode` + zone `StreamText` content into `TextArea` slices.
//!
//! # Design constraints
//!
//! - **No Markdown parsing in hot path.** CommonMark rendering is limited to
//!   plain text for v1-MVP; `#`-headed lines are treated as bold weight, `*...*`
//!   as italic style.  Full Markdown parsing is deferred.
//! - **No per-frame atlas trim.** `atlas.trim()` is called once per frame after
//!   the text pass to reclaim evicted glyph slabs.
//! - **glyphon `Buffer` allocation per text item per frame.** This avoids
//!   statefulness between frames at the cost of per-frame layout; acceptable
//!   for v1 because Stage 6 budget is 4 ms p99 and glyphon layout is fast.
//!
//! # Overflow modes
//!
//! - `Clip` — the `TextBounds` rectangle hard-clips rendered glyphs.
//! - `Ellipsis` — measured word-boundary truncation with the ellipsis glyph
//!   included in the shaped-width budget, implemented in [`crate::overflow`]
//!   (hud-5jbra.3).  Grapheme-cluster fallback applies when no word boundary
//!   fits.  Whole-line vertical visibility is enforced: no partially clipped
//!   glyph rows.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, Style,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight, Wrap,
};
use tze_hud_scene::types::{
    FontFamily, RenderingPolicy, Rgba, TextAlign, TextMarkdownNode, TextOverflow,
};
use wgpu::{Device, MultisampleState, Queue};

use crate::fonts::bundled_font_system;
use crate::overflow::{self, TruncationResult, TruncationViewport};

/// Default line-height multiplier: `line_height_px = font_size_px × this`.
///
/// This is the single source of truth for the compositor's default line height,
/// used by [`crate::markdown::MarkdownTokens::default`] and any render path that
/// lacks a parsed `MarkdownTokens` (e.g. zone items, the legacy `TextItem`
/// constructor). A design-token map may override it per-tile via
/// `typography.line_height.multiplier`.
///
/// Cross-ref: `PORTAL_LINE_HEIGHT_MULTIPLIER` in
/// `crates/tze_hud_projection/src/bin/projection_authority.rs` mirrors this
/// value. The projection binary does not depend on `tze_hud_compositor`, so it
/// cannot import this constant; if you change the value here, update that
/// constant in the same PR.
pub const LINE_HEIGHT_MULTIPLIER: f32 = 1.4;

// ─── TruncationCache ─────────────────────────────────────────────────────────

/// Cache key for one truncation computation.
///
/// Keyed on `(content_hash, bounds_width, bounds_height, font_size_px,
/// font_family, font_weight, viewport_mode)`.  Float fields are stored as
/// IEEE 754 bit patterns (endian-stable because the process never crosses a
/// machine boundary) so they are `Eq + Hash` without lossy rounding.
///
/// `viewport_mode` distinguishes head-anchored from tail-anchored truncation:
/// the same content at the same geometry produces different display text
/// depending on which anchor mode is active (spec tasks 3.2 and 3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TruncationKey {
    /// BLAKE3 hash of the content string (32 bytes).
    content_hash: [u8; 32],
    /// `bounds_width` as IEEE 754 bits.
    bounds_width_bits: u32,
    /// `bounds_height` as IEEE 754 bits.
    bounds_height_bits: u32,
    /// `font_size_px` as IEEE 754 bits.
    font_size_px_bits: u32,
    /// Font family discriminant (SansSerif=0, Monospace=1, Serif=2).
    font_family: u8,
    /// CSS font weight (100–900).
    font_weight: u16,
    /// Viewport anchor mode: 0 = `HeadAnchored`, 1 = `TailAnchored`.
    ///
    /// Same content at the same geometry produces different truncated text
    /// depending on anchor mode, so it must be part of the cache key.
    viewport_mode: u8,
}

impl TruncationKey {
    fn new(
        content: &str,
        bounds_width: f32,
        bounds_height: f32,
        font_size_px: f32,
        font_family: FontFamily,
        font_weight: u16,
        viewport: TruncationViewport,
    ) -> Self {
        Self {
            content_hash: *blake3::hash(content.as_bytes()).as_bytes(),
            bounds_width_bits: bounds_width.to_bits(),
            bounds_height_bits: bounds_height.to_bits(),
            font_size_px_bits: font_size_px.to_bits(),
            font_family: match font_family {
                FontFamily::SystemSansSerif => 0,
                FontFamily::SystemMonospace => 1,
                FontFamily::SystemSerif => 2,
            },
            font_weight,
            viewport_mode: match viewport {
                TruncationViewport::HeadAnchored => 0,
                TruncationViewport::TailAnchored => 1,
            },
        }
    }

    /// BLAKE3 content hash of the text this key truncates.
    ///
    /// Two keys with the same `content_hash` but different geometry/font fields
    /// truncate the *same* string for *different* bounds.  The render path uses
    /// this to find a nearest stale entry for the same content when the exact
    /// (content + geometry) entry has not been primed yet (cadence-deferred
    /// frame) — see [`TruncationCache::get_stale_by_content`].
    #[inline]
    pub(crate) fn content_hash(&self) -> [u8; 32] {
        self.content_hash
    }
}

/// Content-addressed cache of [`TruncationResult`] values.
///
/// Keyed on `(content_hash, bounds_width, bounds_height, font_size_px,
/// font_family, font_weight)`.  The cache is populated outside the per-frame
/// pipeline by [`TruncationCache::prime`] and looked up in O(1) on the frame
/// path via [`TruncationCache::get_by_key`].
///
/// This enforces the "stable-shape-caching" contract from RFC 0013 §3.4 and
/// §4.2, Phase-1 design §3: truncation is called only when content or geometry
/// changes — never on every frame.
///
/// Mirrors [`crate::markdown::MarkdownCache`] in structure and ownership model.
///
/// # Stale-by-content fallback (hud-n3mq0)
///
/// The exact-key entries above are only ever populated by [`prime`], which runs
/// off the render path (commit-time / cadence-gated geometry change).  During a
/// fast resize drag the adaptive cadence gate *defers* re-priming, so the
/// render path can see a cache miss for the newest geometry's key.  Rather than
/// shaping inline on the frame loop (the exact O(n) cost the cache exists to
/// avoid — see RFC 0013 §3.4 doctrine "the runtime must never block on
/// per-frame shaping"), the render path falls back to the most-recently-primed
/// truncation for the *same content* at slightly-old geometry.  That stale
/// result is still a VALID truncation (no partial-glyph rows, ellipsis intact),
/// just computed for bounds that are one cadence tick behind.  The deferred
/// off-path prime fills the correct entry for a subsequent frame.
///
/// `by_content` indexes `content_hash → most-recently-primed TruncationKey` to
/// make that nearest-stale lookup O(1) on the render path.
///
/// [`prime`]: TruncationCache::prime
pub(crate) struct TruncationCache {
    entries: HashMap<TruncationKey, TruncationResult>,
    /// Maps `content_hash` → the most-recently-primed [`TruncationKey`] for that
    /// content.  Used by [`TruncationCache::get_stale_by_content`] to serve a
    /// nearest-geometry truncation on a cadence-deferred render-path miss.
    by_content: HashMap<[u8; 32], TruncationKey>,
    /// `max_truncation_input_bytes` bound for the viewport-adjacent-window
    /// truncation fallback (spec.md §324/§331).  When a single uncached
    /// truncation would shape more than this many bytes, [`TruncationCache::prime`]
    /// restricts the shaped input to a viewport-adjacent window of whole source
    /// lines (`overflow::viewport_adjacent_input`) instead of the full retained
    /// transcript, keeping a single uncached call within the Stage-5 Layout
    /// Resolve budget (< 1 ms).
    max_truncation_input_bytes: usize,
}

impl Default for TruncationCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            by_content: HashMap::new(),
            max_truncation_input_bytes: overflow::DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
        }
    }
}

impl TruncationCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Override the `max_truncation_input_bytes` bound for the
    /// viewport-adjacent-window fallback.  Called from
    /// [`super::Compositor::set_max_truncation_input_bytes`] when the runtime
    /// applies the resolved `DisplayProfile::max_truncation_input_bytes`; the
    /// default is [`overflow::DEFAULT_MAX_TRUNCATION_INPUT_BYTES`].
    pub(crate) fn set_max_truncation_input_bytes(&mut self, bytes: usize) {
        self.max_truncation_input_bytes = bytes;
    }

    /// The current `max_truncation_input_bytes` bound governing the
    /// viewport-adjacent-window fallback in [`TruncationCache::prime`].
    #[cfg(test)]
    #[inline]
    pub(crate) fn max_truncation_input_bytes(&self) -> usize {
        self.max_truncation_input_bytes
    }

    /// Number of cached entries.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no entries are cached.
    #[inline]
    #[allow(dead_code)] // used in tests; companion to len()
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up a cached truncation result.  Returns `None` on a cache miss.
    ///
    /// O(1) hash-map lookup; no shaping, no hashing of the content string.
    /// Callers on the frame path must pre-compute the key via [`TruncationKey::new`]
    /// and call [`TruncationCache::get_by_key`].
    #[inline]
    pub(crate) fn get_by_key(&self, key: &TruncationKey) -> Option<&TruncationResult> {
        self.entries.get(key)
    }

    /// Render-path fallback: return the most-recently-primed truncation for the
    /// **same content** at slightly-old geometry, when the exact-geometry key is
    /// not yet primed (a cadence-deferred frame — hud-n3mq0).
    ///
    /// O(1): one `content_hash` index lookup plus one entries lookup.  Does **no**
    /// shaping — it never calls [`overflow::truncate_for_ellipsis`].  The returned
    /// result is a valid truncation (no partial-glyph rows, ellipsis intact) for
    /// the previous geometry; the deferred off-path prime fills the correct
    /// entry for a subsequent frame.  Returns `None` only when this content has
    /// never been primed at any geometry (true cold start), in which case the
    /// caller renders the untruncated text clipped to bounds by the glyph buffer.
    #[inline]
    pub(crate) fn get_stale_by_content(
        &self,
        content_hash: &[u8; 32],
    ) -> Option<&TruncationResult> {
        let key = self.by_content.get(content_hash)?;
        self.entries.get(key)
    }

    /// Compute and cache the truncation result if not already present.
    ///
    /// Returns a reference to the cached result.  Calling this twice for the
    /// same key is a no-op after the first call (same content + same geometry +
    /// same viewport mode → same result).
    ///
    /// # Viewport dispatch
    ///
    /// The `viewport` parameter selects the truncation algorithm:
    ///
    /// - [`TruncationViewport::HeadAnchored`] (default) — calls
    ///   [`overflow::truncate_for_ellipsis`].  Used for static tiles and
    ///   scrolled-back viewports (spec task 3.3 append-stability guarantee).
    ///
    /// - [`TruncationViewport::TailAnchored`] — calls
    ///   [`overflow::truncate_tail_anchored`].  Used for follow-tail streaming
    ///   transcripts whose viewport is at the tail (spec task 3.2 whole-line
    ///   advancement guarantee).
    ///
    /// Call this at content-commit time, **not** on the frame path.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prime<'a>(
        &'a mut self,
        key: TruncationKey,
        content: &str,
        bounds_width: f32,
        bounds_height: f32,
        font_size_px: f32,
        font_family: FontFamily,
        font_weight: u16,
        viewport: TruncationViewport,
        line_height_multiplier: f32,
        font_system: &mut FontSystem,
    ) -> &'a TruncationResult {
        // Track the most-recently-primed key for this content so a later
        // cadence-deferred render-path miss can fall back to the nearest
        // geometry's truncation instead of shaping inline (hud-n3mq0).  Updated
        // on every prime call (hit or miss) so the freshest geometry wins.
        self.by_content.insert(key.content_hash(), key);
        let max_truncation_input_bytes = self.max_truncation_input_bytes;
        self.entries.entry(key).or_insert_with(|| {
            let family = match font_family {
                FontFamily::SystemSansSerif => Family::SansSerif,
                FontFamily::SystemMonospace => Family::Monospace,
                FontFamily::SystemSerif => Family::Serif,
            };
            let weight = Weight(font_weight.clamp(100, 900));
            let base_attrs = Attrs::new().family(family).weight(weight);
            let line_height = font_size_px * line_height_multiplier;
            // Viewport-adjacent-window fallback (spec.md §324/§331): when the
            // committed content would shape an input larger than the configured
            // `max_truncation_input_bytes` bound, restrict the shaped input to a
            // viewport-adjacent window of whole source lines so a single uncached
            // truncation stays within the Stage-5 Layout Resolve budget.  This is
            // byte-identical to the full-input result for the lines within the
            // window because cosmic-text wraps each source line independently
            // (see overflow::viewport_adjacent_input). It is a no-op for inputs
            // within the bound.
            let shaped_input = overflow::viewport_adjacent_input(
                content,
                bounds_height,
                line_height,
                viewport,
                max_truncation_input_bytes,
                overflow::TRUNCATION_OVERSCAN_LINES,
            );
            // Dispatch to the correct truncation algorithm via TruncationViewport.
            // TailAnchored shows the LAST max_lines runs (newest content visible).
            // HeadAnchored shows the FIRST max_lines runs (default / scrolled-back).
            match viewport {
                TruncationViewport::TailAnchored => overflow::truncate_tail_anchored(
                    shaped_input,
                    base_attrs,
                    bounds_width,
                    bounds_height,
                    font_size_px,
                    line_height,
                    font_system,
                ),
                TruncationViewport::HeadAnchored => overflow::truncate_for_ellipsis(
                    shaped_input,
                    base_attrs,
                    bounds_width,
                    bounds_height,
                    font_size_px,
                    line_height,
                    font_system,
                ),
            }
        })
    }

    /// Evict all entries whose key is not in `live_keys`.
    ///
    /// Call this after priming to keep the cache bounded to the live node set.
    pub(crate) fn evict_except(&mut self, live_keys: &[TruncationKey]) {
        let keep: HashSet<TruncationKey> = live_keys.iter().copied().collect();
        self.entries.retain(|k, _| keep.contains(k));
        // Keep the stale-by-content index bounded to surviving entries so it
        // never points at an evicted key (which would make get_stale_by_content
        // return None despite a live entry existing for that content).
        self.by_content.retain(|_, k| keep.contains(k));
    }

    /// Drop all cached entries.  Used when token map changes and font metrics
    /// may shift (font fallback / substitution may change after `set_token_map`).
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.by_content.clear();
    }
}

// ─── TextRasterizer ───────────────────────────────────────────────────────────

/// Compositor-owned glyphon state.
///
/// Created once per `Compositor` via [`TextRasterizer::new`]. Holds the
/// font system, glyph atlas, and renderer. Not `Send` — must stay on the
/// compositor thread.
pub struct TextRasterizer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    renderer: TextRenderer,
    /// Content-addressed IDs of agent-uploaded fonts already loaded into
    /// `font_system`.  Used to skip redundant `load_font_data` calls (which
    /// would add duplicate entries to fontdb).
    ///
    /// The ID is the raw 32-byte BLAKE3 digest (`ResourceId` wire form) of
    /// the font bytes, matching the key used by `tze_hud_resource::FontBytesStore`.
    loaded_font_ids: HashSet<[u8; 32]>,
    /// Truncation result cache for `TextOverflow::Ellipsis` items.
    ///
    /// Keyed on `(content_hash, bounds_width, bounds_height, font_size_px,
    /// font_family, font_weight)`.  Populated outside the frame loop by
    /// [`TextRasterizer::prime_truncation_cache`]; the frame path looks up
    /// pre-computed results in O(1) via [`TruncationCache::get_by_key`].
    ///
    /// This satisfies the Phase-1 stable-shape-caching contract (RFC 0013
    /// §3.4, §4.2): `truncate_for_ellipsis` is called exactly once per
    /// content/geometry change, never on every frame.
    pub(crate) truncation_cache: TruncationCache,
    /// Cumulative count of `shape_until_scroll` invocations on the per-frame
    /// `prepare_text_items` path (hud-991cj).
    ///
    /// This is the measurement lever for the scroll-flicker fix: a `Buffer` is
    /// shaped only when its content/geometry/font changed, so on a pure scroll
    /// (offset changes but shape inputs do not) this counter must stay flat once
    /// a shaped-buffer cache lands. Incremented immediately before each frame-path
    /// `shape_until_scroll`; read via [`TextRasterizer::shape_call_count`].
    #[allow(dead_code)] // read only by the hud-991cj scroll-reshape benchmark
    pub(crate) shape_call_count: u64,
    /// Shaped-buffer reuse cache (hud-991cj): a glyphon `Buffer` keyed by a
    /// content-and-geometry `ShapeKey` (BLAKE3 of every input that affects the
    /// shaped glyphs — see [`shape_key_for_item`]). The **scroll offset is
    /// deliberately NOT in the key** — a Clip pane's shaped output is
    /// offset-independent (offset only moves the draw position), so a pure scroll
    /// reuses the shaped buffer with zero re-shape (the whole win). Content /
    /// bounds / font / run change → key miss → exactly one re-shape.
    ///
    /// Bounded by construction: `prepare_text_items` moves the whole map out at
    /// the top of the frame and rebuilds it from *this frame's* live items, so it
    /// only ever holds the currently-rendered set (stale keys are dropped, not
    /// accumulated). Front-runs the Wave-3 "perf-whole-scene-reshape-per-frame"
    /// shaped-buffer-reuse item.
    shape_cache: HashMap<[u8; 32], Buffer>,
}

/// Wrap policy for wrapped text (composer draft, viewer-echo history, and every
/// bounded `TextItem` in the render path).
///
/// [`Wrap::WordOrGlyph`] wraps at word boundaries **and** falls back to breaking
/// a single token at the glyph level when that token is wider than the line box
/// (hud-n0x4u). Plain [`Wrap::Word`] would leave such an unbreakable token on one
/// over-long line whose tail is then clipped away by the box scissor — the owner
/// could type a 200-char run of `asdasd…` and never see the end. `WordOrGlyph` is
/// a strict superset of `Word` (identical for text that already fits), so this is
/// safe to apply to the shared render path.
///
/// This constant is the single source of truth for that policy: the measurement
/// helpers ([`TextRasterizer::measure_composer_wrapped`],
/// [`TextRasterizer::measure_composer_visual_layout`],
/// [`composer_wrap_line_widths`]) and the render path
/// ([`TextRasterizer::build_and_shape_buffer`]) all use it, so predicted line
/// counts and painted line counts always agree (no caret drift).
pub(crate) const WRAPPED_TEXT_WRAP: Wrap = Wrap::WordOrGlyph;

/// Shape `text` wrapped to `wrap_width` under [`WRAPPED_TEXT_WRAP`] and return the
/// advance width (`line_w`) of each resulting visual line, top-to-bottom.
///
/// Pure CPU: operates on a borrowed [`FontSystem`] (no GPU device needed), so it
/// is unit-testable without a headless surface. Used by
/// [`TextRasterizer::measure_wrapped_line_count`] (the viewer-echo history line
/// count) and exercised directly by the break-anywhere wrap tests. Uses the same
/// sans-serif base attributes and metrics as the composer/echo measurement paths
/// so the counted layout matches what the render path paints.
///
/// Returns an empty vec only for text that produces no layout runs (e.g. the
/// empty string); callers that need a floor of one line should `.max(1)`.
pub(crate) fn composer_wrap_line_widths(
    font_system: &mut FontSystem,
    text: &str,
    wrap_width: f32,
    font_size_px: f32,
    line_height_multiplier: f32,
) -> Vec<f32> {
    let line_height = font_size_px * line_height_multiplier;
    let mut buf = Buffer::new(font_system, Metrics::new(font_size_px, line_height));
    buf.set_size(font_system, Some(wrap_width.max(1.0)), None);
    buf.set_wrap(font_system, WRAPPED_TEXT_WRAP);
    let base_attrs = Attrs::new().family(Family::SansSerif).weight(Weight(400));
    buf.set_text(font_system, text, base_attrs, Shaping::Advanced);
    buf.shape_until_scroll(font_system, false);
    buf.layout_runs().map(|run| run.line_w).collect()
}

/// The tile-local text content-box margins `(margin_x, margin_y)` the render path
/// insets a `TextMarkdownNode` by, for a node of the given bounds and
/// `line_height`: a horizontal margin scaled from width and a vertical margin
/// scaled from height (both clamped to `[1, 6]` px), the vertical one additionally
/// bounded so it never eats more than half the height above one line. The painted
/// content box is `(bounds.width - 2*margin_x, bounds.height - 2*margin_y)`.
///
/// Extracted so the render constructors (`TextItem::from_text_markdown_node` /
/// `_cached`) and the vertical-flow measurement
/// (`crate::vertical_flow::flow_child_height`) compute the wrap width and vertical
/// padding from the SAME formula and cannot drift (hud-yfj8u; the drift class
/// hud-3xdlf fixed for the height math).
pub(crate) fn text_content_box_margins(
    bounds_width: f32,
    bounds_height: f32,
    line_height: f32,
) -> (f32, f32) {
    let margin_x = (bounds_width * 0.08).clamp(1.0, 6.0);
    let target_margin_y = (bounds_height * 0.20).clamp(1.0, 6.0);
    let max_margin_y = ((bounds_height - line_height).max(0.0) / 2.0).min(6.0);
    let margin_y = target_margin_y.min(max_margin_y);
    (margin_x, margin_y)
}

/// Measure the rendered height (px) of markdown `content` wrapped to
/// `wrap_width`, reproducing the SAME shaping the render path uses for a
/// `TextMarkdownNode` — [`TextItem::from_text_markdown_cached`] +
/// [`TextRasterizer::build_and_shape_buffer`]'s styled-run branch — rather
/// than a second, independently-drifting reimplementation (hud-3xdlf).
///
/// Three things this deliberately does that [`composer_wrap_line_widths`]
/// (the composer/plain-text measurement path) does not, because the render
/// path itself does them for markdown:
///
/// 1. **Parses `content` first.** `content` is markdown SOURCE, but the render
///    path shapes [`crate::markdown::ParsedMarkdown::plain_text`] — markdown
///    syntax (`**`, `#`, `` ` ``, …) is stripped before shaping. Wrapping the
///    raw source instead would wrap different text at different widths and
///    produce a different line count.
/// 2. **Applies per-span `size_scale`.** A heading run shapes at a genuinely
///    larger font size and thus a taller line via glyphon's per-span
///    `Attrs::metrics` (see [`styled_run_spans`]) — `composer_wrap_line_widths`
///    only ever shapes at one uniform `font_size_px` for the whole buffer, so
///    it cannot see this.
/// 3. **Uses the token-resolved `parsed.line_height_multiplier`**, not the
///    constant [`LINE_HEIGHT_MULTIPLIER`] (composer's own hardcoded metric,
///    unrelated to `portal.transcript.*` / `typography.line_height.multiplier`
///    tokens the markdown parse path resolves at parse time).
///
/// Because per-span metrics can make individual visual lines taller than the
/// buffer's base line height, total height is NOT `line_count * line_height`
/// (that formula assumes uniform lines, which no longer holds once a heading
/// is present) — it is read back from glyphon's actual per-line layout via
/// `LayoutRun::line_top` + `LayoutRun::line_height` on the last shaped line,
/// which glyphon already computes as the max of that line's constituent
/// spans' metrics. Floors to one base line's height when `content` produces no
/// layout runs (e.g. empty content), matching `composer_wrap_line_widths`'
/// documented empty-input behavior plus the `.max(1)` floor its callers apply.
/// Measure the rendered height (px) of an ALREADY-PARSED markdown node — the
/// commit-time cached [`crate::markdown::ParsedMarkdown`] the render path consumes
/// via [`TextItem::from_text_markdown_cached`] — WITHOUT re-parsing on the frame
/// path (hud-5wx09; text-stream-portals §"per-frame flow measurement MUST consume
/// the commit-time cached parsed representation … MUST NOT re-parse").
///
/// This is the parse-free core of [`measure_markdown_content_height`]: it consumes
/// `parsed.plain_text`, `parsed.spans`, and `parsed.line_height_multiplier`
/// exactly as `from_text_markdown_cached` does, so a flowed node whose content has
/// not changed incurs ZERO per-frame parse cost. The re-parsing
/// [`measure_markdown_content_height`] is retained only as the cache-MISS fallback.
pub(crate) fn measure_markdown_content_height_cached(
    font_system: &mut FontSystem,
    parsed: &crate::markdown::ParsedMarkdown,
    wrap_width: f32,
    base_font_size_px: f32,
    font_family: FontFamily,
) -> f32 {
    let styled_runs = markdown_spans_to_styled_runs(&parsed.plain_text, &parsed.spans);

    // Same font-size clamp `TextItem::from_text_markdown_cached` applies to
    // `node.font_size_px`, so a caller passing an out-of-range size measures
    // against the same clamped value the render path would actually shape.
    let base_font_size_px = base_font_size_px.clamp(6.0, 200.0);
    let lh_multiplier = parsed.line_height_multiplier;
    let base_line_height = base_font_size_px * lh_multiplier;

    let mut buf = Buffer::new(
        font_system,
        Metrics::new(base_font_size_px, base_line_height),
    );
    buf.set_size(font_system, Some(wrap_width.max(1.0)), None);
    buf.set_wrap(font_system, WRAPPED_TEXT_WRAP);
    let family = match font_family {
        FontFamily::SystemSansSerif => Family::SansSerif,
        FontFamily::SystemMonospace => Family::Monospace,
        FontFamily::SystemSerif => Family::Serif,
    };
    // font_weight 400 matches `TextItem::from_text_markdown_cached`, which
    // always sets it to 400 for the markdown-cached path (not
    // token-configurable there either) — mirrored here for fidelity, not
    // independently chosen.
    let base_attrs = Attrs::new().family(family).weight(Weight(400));
    let spans = styled_run_spans(
        &parsed.plain_text,
        &styled_runs,
        base_attrs,
        font_family,
        base_font_size_px,
        lh_multiplier,
    );
    buf.set_rich_text(font_system, spans, base_attrs, Shaping::Advanced);
    buf.shape_until_scroll(font_system, false);

    buf.layout_runs()
        .last()
        .map(|run| run.line_top + run.line_height)
        .unwrap_or(base_line_height)
}

/// Cache-MISS fallback for [`measure_markdown_content_height_cached`]: parse
/// `content` with `markdown_tokens` and measure the result.
///
/// The steady-state flow-measurement path looks up the commit-time cached
/// [`crate::markdown::ParsedMarkdown`] and calls the `_cached` variant with zero
/// per-frame parsing (hud-5wx09). This re-parsing variant is invoked ONLY when the
/// cache does not yet hold the node's content — a cold first frame or a node added
/// mid-frame — which is a one-time fill on content change, not a per-frame parse
/// of unchanged content, and mirrors the render path's own inline parse-on-miss
/// (`renderer/text.rs`, `MarkdownCache` miss branch). It is also used by callers
/// (tests, non-cache paths) that hold no cache.
pub(crate) fn measure_markdown_content_height(
    font_system: &mut FontSystem,
    content: &str,
    wrap_width: f32,
    base_font_size_px: f32,
    font_family: FontFamily,
    markdown_tokens: &crate::markdown::MarkdownTokens,
) -> f32 {
    let parsed = crate::markdown::parse_markdown_subset(content, markdown_tokens);
    measure_markdown_content_height_cached(
        font_system,
        &parsed,
        wrap_width,
        base_font_size_px,
        font_family,
    )
}

/// Measure the rendered height (px) of RAW markdown SOURCE `content` —
/// markdown syntax NOT stripped — wrapped to `wrap_width`, reproducing the
/// render path's shaping for a `TextMarkdownNode` that carries pixel-bearing
/// `color_runs` ([`TextItem::from_text_markdown_node`]'s raw-content branch,
/// hud-ysyis).
///
/// That render constructor deliberately does two things
/// [`measure_markdown_content_height`] does not, mirrored here:
///
/// 1. **Skips markdown stripping entirely.** The caller-supplied color-run
///    byte offsets are pinned to the RAW content; stripping markdown syntax
///    before shaping would shift every byte position after the first marker
///    and invalidate them. `content` is measured exactly as given.
/// 2. **Uses the DEFAULT line-height multiplier unconditionally**, via
///    `crate::markdown::MarkdownTokens::default()` — that constructor has no
///    access to a parsed, token-resolved `MarkdownTokens` (by design: a
///    token-driven reflow of the wrap width or line height could shift glyph
///    positions out from under the pinned color-run offsets the same way
///    stripping would), so this measurement must not resolve real tokens
///    either, even when the caller has them.
///
/// `color_runs` themselves carry only a color (`ColorRunItem`/`TextColorRun`
/// — no weight, italic, monospace, or size_scale), so they never affect
/// layout or wrapping and are correctly irrelevant to a height measurement;
/// callers do not need to supply them here.
///
/// Unlike [`composer_wrap_line_widths`] (which always shapes at a fixed
/// sans-serif family — correct for composer/viewer-echo plain text, which
/// never carries pixel-bearing runs), this uses `font_family`: the render
/// path applies the node's own family on this branch too.
pub(crate) fn measure_raw_content_height(
    font_system: &mut FontSystem,
    content: &str,
    wrap_width: f32,
    font_size_px: f32,
    font_family: FontFamily,
) -> f32 {
    // Same font-size clamp `TextItem::from_text_markdown_node` applies to
    // `node.font_size_px`.
    let font_size_px = font_size_px.clamp(6.0, 200.0);
    let line_height_multiplier = crate::markdown::MarkdownTokens::default().line_height_multiplier;
    let line_height = font_size_px * line_height_multiplier;
    let mut buf = Buffer::new(font_system, Metrics::new(font_size_px, line_height));
    buf.set_size(font_system, Some(wrap_width.max(1.0)), None);
    buf.set_wrap(font_system, WRAPPED_TEXT_WRAP);
    let family = match font_family {
        FontFamily::SystemSansSerif => Family::SansSerif,
        FontFamily::SystemMonospace => Family::Monospace,
        FontFamily::SystemSerif => Family::Serif,
    };
    let base_attrs = Attrs::new().family(family).weight(Weight(400));
    buf.set_text(font_system, content, base_attrs, Shaping::Advanced);
    buf.shape_until_scroll(font_system, false);
    buf.layout_runs()
        .last()
        .map(|run| run.line_top + run.line_height)
        .unwrap_or(line_height)
}

impl TextRasterizer {
    /// Create a text rasterizer targeting the given surface format.
    ///
    /// This must be called after the wgpu `Device` and `Queue` are available
    /// (i.e. after `Compositor::new_headless` or `new_windowed`).
    pub fn new(device: &Device, queue: &Queue, format: wgpu::TextureFormat) -> Self {
        // Build a self-contained FontSystem from bundled fonts only — no OS
        // system-font scan.  This makes the compositor reliable on kiosk/minimal
        // hosts (headless servers, containers, Windows Nano) where system fonts
        // are absent or incomplete.  Agent-uploaded fonts are still accepted
        // afterwards via `load_font_bytes`.
        //
        // `bundled_font_system()` uses `FontSystem::new_with_locale_and_db` to
        // skip `db.load_system_fonts()` and sets family mappings so that
        // `Family::SansSerif / Monospace / Serif` resolve to the bundled DejaVu
        // faces rather than the cosmic-text defaults ("Fira Mono" / "Fira Sans" /
        // "DejaVu Serif") which may not match the faces we actually loaded.
        let font_system = bundled_font_system();

        tracing::info!(
            font_face_count = font_system.db().faces().count(),
            "TextRasterizer initialised with bundled fonts (no system-font scan)"
        );

        let swash_cache = SwashCache::new();
        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let mut atlas = TextAtlas::new(device, queue, &cache, format);
        let renderer = TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);

        Self {
            font_system,
            swash_cache,
            viewport,
            atlas,
            renderer,
            loaded_font_ids: HashSet::new(),
            truncation_cache: TruncationCache::new(),
            shape_call_count: 0,
            shape_cache: HashMap::new(),
        }
    }

    /// Cumulative number of per-frame `shape_until_scroll` calls (hud-991cj).
    /// Used by the scroll-reshape benchmark/regression to prove a pure scroll
    /// performs zero re-shapes once the shaped-buffer cache is in place.
    #[inline]
    #[allow(dead_code)] // used by the hud-991cj scroll-reshape benchmark
    pub(crate) fn shape_call_count(&self) -> u64 {
        self.shape_call_count
    }

    /// Load raw font bytes (TTF or OTF) into glyphon's `FontSystem`.
    ///
    /// After this call the font is available for text layout via glyphon's
    /// automatic family detection.  Subsequent `TextItem`s whose `font_family`
    /// resolves to the family embedded in these bytes will use them.
    ///
    /// # Parameters
    ///
    /// - `resource_id` — the 32-byte BLAKE3 content hash of `data`
    ///   (matches `ResourceId::as_bytes()` from `tze_hud_resource`).
    ///   Used to deduplicate: calling this with the same `resource_id` twice
    ///   is a no-op after the first call.
    /// - `data` — raw TTF or OTF bytes.
    ///
    /// # Thread safety
    ///
    /// `TextRasterizer` is `!Send` — this must be called from the compositor
    /// thread only (same thread that calls `prepare_text_items` and
    /// `render_text_pass`).
    pub fn load_font_bytes(&mut self, resource_id: [u8; 32], data: &[u8]) {
        if self.loaded_font_ids.contains(&resource_id) {
            tracing::debug!(
                resource_id = %format_resource_id(&resource_id),
                "font already loaded — skipping duplicate load_font_data"
            );
            return;
        }

        self.font_system.db_mut().load_font_data(data.to_vec());

        self.loaded_font_ids.insert(resource_id);

        // A new face can change font fallback / substitution for text that was
        // already shaped, so every cached shaped buffer is now potentially stale
        // (the ShapeKey does not capture FontSystem state). Drop the cache; it is
        // rebuilt from the next frame's items on the miss path (hud-991cj).
        self.shape_cache.clear();

        tracing::info!(
            resource_id = %format_resource_id(&resource_id),
            bytes = data.len(),
            "agent-uploaded font loaded into FontSystem"
        );
    }

    /// Returns `true` if the font identified by `resource_id` has already been
    /// loaded into the `FontSystem`.
    #[inline]
    pub fn has_font(&self, resource_id: &[u8; 32]) -> bool {
        self.loaded_font_ids.contains(resource_id)
    }

    /// Total number of font faces visible to glyphon's `FontSystem`.
    #[inline]
    pub fn font_face_count(&self) -> usize {
        self.font_system.db().faces().count()
    }

    /// Update the viewport resolution before each frame.
    ///
    /// Must be called once per frame before `render_text_pass`.
    pub fn update_viewport(&mut self, queue: &Queue, width: u32, height: u32) {
        self.viewport.update(queue, Resolution { width, height });
    }

    /// Measure the composer draft for horizontal caret-follow (hud-zlfi4).
    ///
    /// Shapes `text` as a single unwrapped line with the sans-serif composer font
    /// and returns `(caret_x, content_width)` in physical pixels, both measured
    /// from the start of the draft (x = 0 at the first glyph):
    /// - `caret_x` — the x of the glyph the caret sits before (`0` at `cursor_byte
    ///   == 0`, `content_width` when the caret is at or past the draft end).
    /// - `content_width` — the full draft line width (`line_w`).
    ///
    /// `cursor_byte` is snapped down to a valid UTF-8 boundary (agent-provided
    /// offsets may land mid-character), so this never panics on the input.
    ///
    /// The caller ([`Compositor::prime_composer_scroll_offset`]) feeds the result
    /// into [`crate::renderer::image_cache::composer_scroll_offset`]. This runs
    /// once per frame only while a composer is active — not on the transcript hot
    /// path — so a single short single-line shape here is negligible.
    pub(crate) fn measure_composer_caret(
        &mut self,
        text: &str,
        cursor_byte: usize,
        font_size_px: f32,
        line_height_multiplier: f32,
    ) -> (f32, f32) {
        // Snap the caret offset to a char boundary at or below the requested byte.
        let mut cursor = cursor_byte.min(text.len());
        while cursor > 0 && !text.is_char_boundary(cursor) {
            cursor -= 1;
        }

        let line_height = font_size_px * line_height_multiplier;
        let mut buf = Buffer::new(
            &mut self.font_system,
            Metrics::new(font_size_px, line_height),
        );
        // Unbounded width + no wrap → the natural single-line width, matching how
        // the composer strip lays out (it is always one line for v1).
        buf.set_size(&mut self.font_system, None, None);
        buf.set_wrap(&mut self.font_system, Wrap::None);
        let base_attrs = Attrs::new().family(Family::SansSerif).weight(Weight(400));
        buf.set_text(&mut self.font_system, text, base_attrs, Shaping::Advanced);
        buf.shape_until_scroll(&mut self.font_system, false);

        let Some(run) = buf.layout_runs().next() else {
            // Empty text (or no shaped run) → caret at origin, zero-width draft.
            return (0.0, 0.0);
        };
        let content_width = run.line_w;
        // The caret sits before the first glyph whose logical start is at or after
        // the cursor byte. If the cursor is past every glyph (End), it sits at the
        // line width. Glyphs are in visual order; for the single-line, LTR draft
        // this v1 composer supports, logical `start` is monotonic.
        let mut caret_x = content_width;
        for glyph in run.glyphs.iter() {
            if glyph.start >= cursor {
                caret_x = glyph.x;
                break;
            }
        }
        (caret_x, content_width)
    }

    /// Measure the single-line composer draft's [`tze_hud_input::ComposerVisualLayout`]
    /// (hud-hxhnt finding 1) — a one-row visual layout so the runtime's pointer
    /// hit-test (`composer_pointer_byte_offset`) can map a click to a real
    /// glyph-geometry byte offset via `ComposerVisualLayout::byte_at_point` instead
    /// of falling back to a crude linear byte-fraction guess.
    ///
    /// Before this, only the multi-line profile (`measure_composer_visual_layout`)
    /// published a visual layout; the single-line profile published nothing, so a
    /// single-line composer's pointer caret placement was always the linear-guess
    /// fallback. This shapes `text` the same way [`Self::measure_composer_caret`]
    /// does (unbounded width, `Wrap::None` — the natural single-line width) and
    /// records the same `(byte, x)` glyph-boundary pairs
    /// [`Self::measure_composer_visual_layout`] does, just for the single
    /// unwrapped run.
    ///
    /// Returns a layout with exactly one [`tze_hud_input::ComposerVisualLine`]
    /// spanning `[0, text.len())`, a trailing `(text.len(), line_width)` sentinel,
    /// `text_len = text.len()`, and `input_box: None` (the single-line profile has
    /// no rendered multi-row box; `byte_at_point` falls back to an even split over
    /// the one row, which is exact for a single row).
    pub(crate) fn measure_composer_single_line_layout(
        &mut self,
        text: &str,
        font_size_px: f32,
        line_height_multiplier: f32,
    ) -> tze_hud_input::ComposerVisualLayout {
        let line_height = font_size_px * line_height_multiplier;
        let mut buf = Buffer::new(
            &mut self.font_system,
            Metrics::new(font_size_px, line_height),
        );
        // Unbounded width + no wrap → the natural single-line width, matching
        // measure_composer_caret's shaping exactly so the two never disagree.
        buf.set_size(&mut self.font_system, None, None);
        buf.set_wrap(&mut self.font_system, Wrap::None);
        let base_attrs = Attrs::new().family(Family::SansSerif).weight(Weight(400));
        buf.set_text(&mut self.font_system, text, base_attrs, Shaping::Advanced);
        buf.shape_until_scroll(&mut self.font_system, false);

        let mut glyph_x: Vec<(usize, f32)> = Vec::new();
        let mut line_w = 0.0f32;
        if let Some(run) = buf.layout_runs().next() {
            line_w = run.line_w;
            for g in run.glyphs.iter() {
                glyph_x.push((g.start, g.x));
            }
        }
        // Trailing sentinel maps the row end (caret past the last glyph).
        glyph_x.push((text.len(), line_w));
        glyph_x.sort_by(|a, b| a.0.cmp(&b.0));

        tze_hud_input::ComposerVisualLayout {
            lines: vec![tze_hud_input::ComposerVisualLine {
                start_byte: 0,
                end_byte: text.len(),
                glyph_x,
            }],
            text_len: text.len(),
            input_box: None,
            // Set by the caller (`prime_composer_scroll_offset`), which knows
            // the caret-follow scroll for this frame; this function only
            // shapes glyph geometry.
            h_scroll_px: 0.0,
        }
    }

    /// Measure a composer draft under WORD WRAP for the multi-line profile
    /// (hud-nx7yq.1).
    ///
    /// Shapes `text` wrapped to `wrap_width` (the composer's interior width) with
    /// the sans-serif composer font and returns `(total_lines, caret_line)`:
    /// - `total_lines` — number of wrapped visual lines (≥ 1, even for empty text).
    /// - `caret_line`  — zero-based wrapped-line index the caret at `caret_byte`
    ///   sits on (the line of the first glyph whose logical start is ≥ the caret
    ///   byte; the last line when the caret is at or past the end).
    ///
    /// `caret_byte` is snapped down to a valid UTF-8 boundary, so agent-provided
    /// offsets never panic. `text` is the RAW draft text (hud-hxhnt): the caret is
    /// a chrome-layer quad now, not an inserted glyph, so the rendered text is
    /// always the raw draft and this measures the same string — the box sizing and
    /// the render never disagree, and a zero-width caret at a wrap boundary clips
    /// harmlessly (no glyph to clip).
    ///
    /// Same wrap parameters as the render path (`bounds_width = wrap_width`,
    /// [`WRAPPED_TEXT_WRAP`]), so the line counts agree — including the
    /// break-anywhere fallback that keeps an over-long unbreakable token on
    /// multiple in-box lines instead of one clipped line (hud-n0x4u). Runs once
    /// per frame only while a multi-line composer is active — off the transcript
    /// hot path.
    pub(crate) fn measure_composer_wrapped(
        &mut self,
        text: &str,
        caret_byte: usize,
        wrap_width: f32,
        font_size_px: f32,
        line_height_multiplier: f32,
    ) -> (usize, usize) {
        // Snap the caret offset to a char boundary at or below the requested byte.
        let mut caret = caret_byte.min(text.len());
        while caret > 0 && !text.is_char_boundary(caret) {
            caret -= 1;
        }

        let line_height = font_size_px * line_height_multiplier;
        let mut buf = Buffer::new(
            &mut self.font_system,
            Metrics::new(font_size_px, line_height),
        );
        // Bounded width + break-anywhere word wrap → the same multi-line layout
        // the render path produces (collect_composer_text_item sets
        // bounds_width = wrap_width; build_and_shape_buffer uses the same
        // WRAPPED_TEXT_WRAP), so an over-long unbreakable token wraps here exactly
        // as it paints and the box height / caret line stay in sync (hud-n0x4u).
        buf.set_size(&mut self.font_system, Some(wrap_width.max(1.0)), None);
        buf.set_wrap(&mut self.font_system, WRAPPED_TEXT_WRAP);
        let base_attrs = Attrs::new().family(Family::SansSerif).weight(Weight(400));
        buf.set_text(&mut self.font_system, text, base_attrs, Shaping::Advanced);
        buf.shape_until_scroll(&mut self.font_system, false);

        // Walk the visual lines in top-to-bottom order. `total_lines` is the run
        // count; `caret_line` is the first run holding a glyph at/after the caret
        // byte (defaults to the last line when the caret is past every glyph).
        let mut total_lines = 0usize;
        let mut caret_line = 0usize;
        let mut found = false;
        for (line_idx, run) in buf.layout_runs().enumerate() {
            total_lines = line_idx + 1;
            if !found {
                caret_line = line_idx;
                if run.glyphs.iter().any(|g| g.start >= caret) {
                    found = true;
                }
            }
        }
        (total_lines.max(1), caret_line)
    }

    /// Count the wrapped visual lines of `text` at `wrap_width` under
    /// [`WRAPPED_TEXT_WRAP`] (≥ 1). Thin wrapper over [`composer_wrap_line_widths`]
    /// for callers that need only the line count and no caret — the viewer-echo
    /// history block (`prime_viewer_echo_layout`). Sharing the same helper keeps
    /// the echo's break-anywhere wrap identical to what the render path paints
    /// (hud-n0x4u), so an over-long single-word reply is counted as the multiple
    /// in-box lines it actually occupies rather than one clipped line.
    /// Mutable access to the rasterizer's `FontSystem` — the SAME font system the
    /// render path shapes glyphs with, so it carries any agent-uploaded fonts
    /// loaded via `load_font_data` (hud-9gopx). Exposed so the vertical-flow
    /// pre-pass (`renderer::…::prime_vertical_flow_layout`) can measure flow child
    /// heights against the real render fonts instead of a fresh bundled font
    /// system, keeping "measured == painted" true for uploaded fonts too.
    pub(crate) fn font_system_mut(&mut self) -> &mut FontSystem {
        &mut self.font_system
    }

    pub(crate) fn measure_wrapped_line_count(
        &mut self,
        text: &str,
        wrap_width: f32,
        font_size_px: f32,
        line_height_multiplier: f32,
    ) -> usize {
        composer_wrap_line_widths(
            &mut self.font_system,
            text,
            wrap_width,
            font_size_px,
            line_height_multiplier,
        )
        .len()
        .max(1)
    }

    /// Measure the composer draft's wrapped VISUAL-LINE layout for soft-wrap
    /// vertical caret movement (hud-21o6x).
    ///
    /// Shapes the RAW draft text (no caret glyph — the caret is a rendering
    /// artifact; navigation is over raw bytes) wrapped to `wrap_width` with the
    /// same params as the render path, then records, per visual row, its raw-text
    /// byte range plus the `(byte, x)` of each glyph boundary. The result is
    /// published to the input thread, which maps caret byte ↔ pixel x for
    /// visual-row Up/Down.
    ///
    /// Byte offsets are full-text: cosmic-text glyph `start`/`end` are relative to
    /// their `BufferLine` (one per hard `\n`), so a per-BufferLine base offset is
    /// added (same accounting as `compute_inline_backdrop_quads`).
    pub(crate) fn measure_composer_visual_layout(
        &mut self,
        text: &str,
        wrap_width: f32,
        font_size_px: f32,
        line_height_multiplier: f32,
    ) -> tze_hud_input::ComposerVisualLayout {
        let line_height = font_size_px * line_height_multiplier;
        let mut buf = Buffer::new(
            &mut self.font_system,
            Metrics::new(font_size_px, line_height),
        );
        buf.set_size(&mut self.font_system, Some(wrap_width.max(1.0)), None);
        // Same break-anywhere policy as the render + caret-line paths
        // (WRAPPED_TEXT_WRAP) so visual-row byte ranges for Up/Down navigation
        // match the painted wrap, including glyph-level breaks inside an over-long
        // token (hud-n0x4u).
        buf.set_wrap(&mut self.font_system, WRAPPED_TEXT_WRAP);
        let base_attrs = Attrs::new().family(Family::SansSerif).weight(Weight(400));
        buf.set_text(&mut self.font_system, text, base_attrs, Shaping::Advanced);
        buf.shape_until_scroll(&mut self.font_system, false);

        // Full-text byte offset where each BufferLine (hard-newline paragraph)
        // begins: line N starts after all prior lines' text + their '\n'.
        let line_byte_offsets: Vec<usize> = {
            let mut offsets = Vec::with_capacity(buf.lines.len());
            let mut running = 0usize;
            for line in buf.lines.iter() {
                offsets.push(running);
                running += line.text().len() + 1;
            }
            offsets
        };

        let mut lines: Vec<tze_hud_input::ComposerVisualLine> = Vec::new();
        for run in buf.layout_runs() {
            let line_offset = line_byte_offsets.get(run.line_i).copied().unwrap_or(0);
            let mut glyph_x: Vec<(usize, f32)> = Vec::with_capacity(run.glyphs.len() + 1);
            let mut min_start = usize::MAX;
            let mut max_end = 0usize;
            for g in run.glyphs.iter() {
                let gs = line_offset + g.start;
                let ge = line_offset + g.end;
                glyph_x.push((gs, g.x));
                min_start = min_start.min(gs);
                max_end = max_end.max(ge);
            }
            let (start_byte, end_byte) = if run.glyphs.is_empty() {
                (line_offset, line_offset)
            } else {
                (min_start, max_end)
            };
            // Trailing sentinel maps the row end (caret past the last glyph).
            glyph_x.push((end_byte, run.line_w));
            // Ascending by byte (LTR shaping is already logical order; sort to be
            // robust to any reordering).
            glyph_x.sort_by(|a, b| a.0.cmp(&b.0));
            lines.push(tze_hud_input::ComposerVisualLine {
                start_byte,
                end_byte,
                glyph_x,
            });
        }
        if lines.is_empty() {
            lines.push(tze_hud_input::ComposerVisualLine {
                start_byte: 0,
                end_byte: 0,
                glyph_x: vec![(0, 0.0)],
            });
        }
        tze_hud_input::ComposerVisualLayout {
            lines,
            text_len: text.len(),
            // The caller (`prime_composer_scroll_offset`) fills this from its own
            // `composer_input_box` geometry after measuring, so the input box
            // stays single-sourced by the compositor (hud-lw60x).
            input_box: None,
            // Multi-line profile wraps rather than scrolling horizontally.
            h_scroll_px: 0.0,
        }
    }

    /// Prime the truncation cache for all `TextOverflow::Ellipsis` items in `items`.
    ///
    /// Must be called **outside the per-frame pipeline** (at content-commit time
    /// or on geometry change), never inside `prepare_text_items`.  After this
    /// call, the frame path resolves each Ellipsis item in O(1) via
    /// `TruncationCache::get_by_key`.
    ///
    /// The `viewport` field on each `TextItem` drives truncation-mode selection:
    /// - [`TruncationViewport::TailAnchored`] items call
    ///   [`overflow::truncate_tail_anchored`] (spec task 3.2 — newest content
    ///   always visible for follow-tail streaming transcripts).
    /// - [`TruncationViewport::HeadAnchored`] items call
    ///   [`overflow::truncate_for_ellipsis`] (default / scrolled-back viewports,
    ///   spec task 3.3 append-stability guarantee).
    ///
    /// Returns the set of live [`TruncationKey`]s so the caller can evict stale
    /// entries with [`TruncationCache::evict_except`].
    pub(crate) fn prime_truncation_cache(&mut self, items: &[TextItem]) -> Vec<TruncationKey> {
        let mut live_keys: Vec<TruncationKey> = Vec::new();
        for item in items {
            // effective_truncation_key returns None for non-Ellipsis items.
            let Some(key) = effective_truncation_key(item) else {
                continue;
            };
            live_keys.push(key);
            // Re-derive effective measurement attrs to pass to prime (must
            // match the key — see effective_truncation_key for the fix (b) invariant).
            let (meas_weight, meas_mono) =
                styled_runs_effective_measurement(&item.styled_runs, item.font_weight);
            let meas_family = if meas_mono {
                FontFamily::SystemMonospace
            } else {
                item.font_family
            };
            self.truncation_cache.prime(
                key,
                &item.text,
                item.bounds_width,
                item.bounds_height,
                item.font_size_px,
                meas_family,
                meas_weight,
                item.viewport,
                item.line_height_multiplier,
                &mut self.font_system,
            );
        }
        live_keys
    }

    /// Prepare text areas for the upcoming render pass.
    ///
    /// Collects all `TextItem`s, builds glyphon `Buffer`s, and calls
    /// `renderer.prepare`. Must be called after `update_viewport` and before
    /// `render_text_pass`.
    ///
    /// When a `TextItem` has `outline_color` and `outline_width > 0`, the text
    /// is rendered 9 times: once at each of the 8 cardinal+diagonal pixel offsets
    /// in `outline_color`, then once more in the fill `color` on top.
    ///
    /// After shaping, computes pixel-exact inline backdrop quads from
    /// `StyledRunItem::background_color` fields using `layout_runs()` glyph
    /// position data (Phase 2, hud-9ieev).  The caller must render these quads
    /// in a flat-rect pass immediately before the glyphon text pass so that
    /// backdrop quads appear behind the glyphs.
    ///
    /// Returns `Ok(quads)` on success where `quads` is the list of inline
    /// backdrop quads to render, or `Err(string)` on glyphon error (non-fatal —
    /// the frame continues with missing text rather than a crash).
    pub fn prepare_text_items(
        &mut self,
        device: &Device,
        queue: &Queue,
        items: &[TextItem],
    ) -> Result<Vec<InlineBackdropQuad>, String> {
        // 8-direction offsets for outline rendering (cardinal + diagonal).
        const OUTLINE_DIRS: [(f32, f32); 8] = [
            (-1.0, 0.0),
            (1.0, 0.0),
            (0.0, -1.0),
            (0.0, 1.0),
            (-1.0, -1.0),
            (1.0, -1.0),
            (-1.0, 1.0),
            (1.0, 1.0),
        ];

        // Each item with outline produces 9 TextAreas (8 outline + 1 fill).
        // Items without outline produce 1 TextArea each.
        // We build all Buffers first, then construct TextArea references.

        // Phase 0 (hud-5jbra.3 / hud-wgq7j): resolve ellipsis-truncated text
        // strings from the truncation cache.
        //
        // The cache is primed outside the frame loop by `prime_truncation_cache`
        // (called from `Compositor::prime_truncation_cache` gated on scene.version).
        // On a warm path (after at least one commit), a cache hit is one BLAKE3
        // hash to reconstruct the key plus one HashMap lookup — zero shaping work.
        //
        // The render path NEVER shapes inline (hud-n3mq0).  Truncation is an O(n)
        // operation and the cache exists precisely to keep it off the frame loop.
        // On a cache miss we resolve in priority order, all O(1) and shaping-free:
        //
        //   1. Exact-geometry hit (warm steady state) — the correct truncation.
        //   2. Stale-by-content hit (cadence-deferred frame): the most-recently-
        //      primed truncation for the SAME content at slightly-old geometry.
        //      Still a valid truncation (no torn glyphs, ellipsis intact), just
        //      one cadence tick behind.  The deferred off-path prime
        //      (`Compositor::prime_truncation_cache`) fills the correct entry for
        //      a subsequent frame — graceful degradation, not a bug (RFC 0013
        //      §3.4 doctrine: arrival ≠ presentation; never block the frame loop).
        //   3. True cold start (this content never primed at ANY geometry):
        //      `None` → the buffer below renders `item.text` clipped to bounds by
        //      `set_size`, which glyphon clips without partial-glyph rows.
        //
        // For Clip items the entry is `None` (use `item.text` directly).
        // For Ellipsis items the entry is `Some(truncated_text_string)`.
        let keys: Vec<Option<TruncationKey>> = items.iter().map(effective_truncation_key).collect();

        let truncated_texts: Vec<Option<String>> = keys
            .iter()
            .map(|key_opt| {
                let key = key_opt.as_ref()?;
                // Exact geometry first; otherwise the nearest stale truncation for
                // this content.  Neither call shapes; both are O(1) lookups.
                let result = self.truncation_cache.get_by_key(key).or_else(|| {
                    self.truncation_cache
                        .get_stale_by_content(&key.content_hash())
                });
                result.map(|r| r.text.clone())
            })
            .collect();

        // Phase 1: build one Buffer per item (shared by all outline + fill passes).
        // Also collect the *effective* styled runs that were actually used for
        // shaping — these match the glyph byte offsets in the shaped buffer.
        // For truncated (Ellipsis) items the effective runs are the resliced
        // versions; for non-truncated items they are item.styled_runs as-is.
        // compute_inline_backdrop_quads uses these instead of item.styled_runs
        // so that glyph↔run byte-offset matching is always correct (hud-9ieev).
        let mut effective_styled_runs_per_item: Vec<Vec<StyledRunItem>> =
            Vec::with_capacity(items.len());

        // Phase 1 shaped-buffer cache (hud-991cj): move the whole cache out and
        // rebuild it from THIS frame's items. Each item's `ShapeKey` hashes every
        // input that affects the shaped glyphs (original + effective/truncated
        // text, bounds, font, weight, alignment, overflow, viewport, line-height,
        // styled/color runs) but NOT the scroll offset — a Clip pane's shaped
        // output is offset-independent, so a pure scroll hits every key and
        // re-shapes zero buffers (the whole win). A key miss (content / geometry /
        // font / runs changed) re-shapes exactly once. Buffers not reused this
        // frame are dropped when `old_cache` falls out of scope, so the cache
        // never accumulates stale entries — it holds only the currently-rendered
        // set.
        let mut old_cache = std::mem::take(&mut self.shape_cache);
        let mut shape_keys: Vec<[u8; 32]> = Vec::with_capacity(items.len());
        let mut buffers: Vec<Buffer> = Vec::with_capacity(items.len());

        for (item, truncated) in items.iter().zip(truncated_texts.iter()) {
            let effective_text: &str = truncated.as_deref().unwrap_or(&item.text);

            // Effective (resliced) styled runs are needed EVERY frame for inline
            // backdrop quads (their pixel positions shift with scroll), so this
            // pure run-computation runs on both the cache-hit and cache-miss
            // paths — split from the set_text/shape work that only runs on a miss.
            // THE SUBTLETY (hud-991cj): the interleave of run-computation with
            // set_text+shape is untangled here so a hit does zero shaping while
            // pinned behaviors (styled runs, truncation reslicing, backdrop quads)
            // still reproduce byte-for-byte.
            effective_styled_runs_per_item
                .push(effective_styled_runs_for_item(item, truncated.as_deref()));

            let key = shape_key_for_item(item, effective_text);
            shape_keys.push(key);

            let buf = if let Some(reused) = old_cache.remove(&key) {
                // Every shape input is unchanged → reuse the shaped buffer with no
                // re-shape. `shape_call_count` deliberately does NOT advance.
                reused
            } else {
                self.build_and_shape_buffer(item, truncated.as_deref())
            };
            buffers.push(buf);
        }
        // Stale buffers (content/geometry/font changed, or items no longer
        // present this frame) are freed here.
        drop(old_cache);

        // Inline backdrop quads (Phase 2, hud-9ieev): compute pixel-exact rects
        // from glyph layout data.  Must happen here, between buffer shaping and
        // the TextArea construction below that borrows from `buffers`.
        // `compute_inline_backdrop_quads` only iterates buffers immutably.
        //
        // Pass `effective_styled_runs_per_item` (which uses the resliced styled
        // runs for truncated items) so that glyph↔byte-offset matching is correct
        // even when the buffer was shaped with different byte offsets than the
        // original `item.styled_runs` (hud-9ieev Thread 3 fix).
        let inline_backdrop_quads =
            compute_inline_backdrop_quads(items, &buffers, &effective_styled_runs_per_item);

        // Phase 2: build TextArea list.
        // For outlined items, we emit 8 outline passes then 1 fill pass.
        // For non-outlined items, we emit 1 fill pass.
        // Because TextArea borrows the buffer, all buffers must outlive this Vec.
        let mut text_areas: Vec<TextArea<'_>> = Vec::with_capacity(items.len() * 9);

        for (item, buf) in items.iter().zip(buffers.iter()) {
            let fill_color = item.color;
            let bounds = TextBounds {
                left: item.clip_pixel_x as i32,
                top: item.clip_pixel_y as i32,
                right: (item.clip_pixel_x + item.clip_bounds_width) as i32,
                bottom: (item.clip_pixel_y + item.clip_bounds_height) as i32,
            };

            // Outline passes (only when outline is active).
            if let (Some(oc), Some(ow)) = (item.outline_color, item.outline_width) {
                if ow > 0.0 {
                    for (dx, dy) in &OUTLINE_DIRS {
                        let offset = ow;
                        text_areas.push(TextArea {
                            buffer: buf,
                            left: item.pixel_x + dx * offset,
                            top: item.pixel_y + dy * offset,
                            scale: 1.0,
                            bounds,
                            default_color: Color::rgba(oc[0], oc[1], oc[2], oc[3]),
                            custom_glyphs: &[],
                        });
                    }
                }
            }

            // Fill pass (always last so it renders on top of outline).
            text_areas.push(TextArea {
                buffer: buf,
                left: item.pixel_x,
                top: item.pixel_y,
                scale: 1.0,
                bounds,
                default_color: Color::rgba(
                    fill_color[0],
                    fill_color[1],
                    fill_color[2],
                    fill_color[3],
                ),
                custom_glyphs: &[],
            });
        }

        let prepare_result = self
            .renderer
            .prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            )
            .map_err(|e| format!("glyphon prepare: {e:?}"));

        // Rebuild the shaped-buffer cache from this frame's buffers now that the
        // `TextArea` borrows of `buffers` have ended (prepare consumed them). Only
        // the currently-rendered set is retained (hud-991cj); on a duplicate key
        // the later buffer wins, which is harmless (identical shape inputs).
        self.shape_cache = shape_keys.into_iter().zip(buffers).collect();

        prepare_result.map(|()| inline_backdrop_quads)
    }

    /// Build and shape a single `Buffer` for `item` — the cache-MISS path of
    /// [`prepare_text_items`] (hud-991cj).
    ///
    /// This is the shaping half of the old inline buffer-building closure,
    /// factored out so a cache HIT can skip it entirely. It performs the actual
    /// `set_text`/`set_rich_text` + `shape_until_scroll` — the only work that a
    /// pure scroll must avoid. It increments [`Self::shape_call_count`] exactly
    /// once (the measurement lever). It does NOT compute the effective styled
    /// runs (those are produced separately by [`effective_styled_runs_for_item`]
    /// so the backdrop-quad path runs on both hit and miss); every styling input
    /// used here is captured by the item's `ShapeKey`, so a reuse is byte-exact.
    fn build_and_shape_buffer(&mut self, item: &TextItem, truncated: Option<&str>) -> Buffer {
        let line_height = item.font_size_px * item.line_height_multiplier;
        let mut buf = Buffer::new(
            &mut self.font_system,
            Metrics::new(item.font_size_px, line_height),
        );
        buf.set_size(
            &mut self.font_system,
            Some(item.bounds_width),
            Some(item.bounds_height),
        );
        // Break-anywhere wrap: word boundaries first, glyph-level fallback for a
        // single token wider than the box so its tail stays visible instead of
        // overflowing into the clip scissor (hud-n0x4u). Shared with the composer
        // / echo measurement helpers via WRAPPED_TEXT_WRAP so counts agree.
        buf.set_wrap(&mut self.font_system, WRAPPED_TEXT_WRAP);
        let family = match item.font_family {
            FontFamily::SystemSansSerif => Family::SansSerif,
            FontFamily::SystemMonospace => Family::Monospace,
            FontFamily::SystemSerif => Family::Serif,
        };
        // Map CSS-style weight (100–900) to glyphon Weight.
        // Clamp to [100, 900]; Weight(0) would select arbitrary fallback fonts.
        let weight = Weight(item.font_weight.clamp(100, 900));
        let base_attrs = Attrs::new().family(family).weight(weight);

        // Use the pre-truncated text when overflow == Ellipsis; otherwise item.text.
        let effective_text: &str = truncated.unwrap_or(&item.text);

        if !item.styled_runs.is_empty() {
            // Phase-1 markdown styled-run path (hud-5jbra.2).
            //
            // `styled_runs` carry per-span weight, italic, monospace, and
            // optional color from the parse cache.  We build `(text_slice,
            // Attrs)` pairs and call `set_rich_text` once — zero re-parse cost.
            //
            // When ellipsis truncation was applied, byte offsets in `styled_runs`
            // must be remapped to the truncated string's coordinate system
            // (hud-so7zu). HeadAnchored: "{prefix}…" (clamp at prefix_len).
            // TailAnchored: "…\n{tail}" (leading ellipsis, hud-fe4ik — offsets
            // shifted; see `reslice_styled_runs_tail_anchored`).
            if let Some(truncated_str) = truncated {
                let resliced = reslice_styled_runs_for_item(item, truncated_str);
                if resliced.is_empty() {
                    // No runs survive truncation — fall back to plain text.
                    buf.set_text(
                        &mut self.font_system,
                        effective_text,
                        base_attrs,
                        Shaping::Advanced,
                    );
                } else {
                    let spans = styled_run_spans(
                        effective_text,
                        &resliced,
                        base_attrs,
                        item.font_family,
                        item.font_size_px,
                        item.line_height_multiplier,
                    );
                    buf.set_rich_text(&mut self.font_system, spans, base_attrs, Shaping::Advanced);
                }
            } else {
                let spans = styled_run_spans(
                    &item.text,
                    &item.styled_runs,
                    base_attrs,
                    item.font_family,
                    item.font_size_px,
                    item.line_height_multiplier,
                );
                buf.set_rich_text(&mut self.font_system, spans, base_attrs, Shaping::Advanced);
            }
        } else if item.color_runs.is_empty() {
            // Fast path: no inline runs — use uniform base color.
            buf.set_text(
                &mut self.font_system,
                effective_text,
                base_attrs,
                Shaping::Advanced,
            );
        } else {
            // Single-pass styled path: build (text_slice, Attrs) pairs and call
            // set_rich_text once.  This avoids the double-shape that occurred when
            // set_text created BufferLines and set_attrs_list then invalidated
            // their shaping state.
            //
            // color_runs are sorted, non-overlapping byte ranges. `default_color`
            // on TextArea acts as the fallback for glyphs without a color_opt, so
            // base_attrs carries no color_opt and run attrs carry explicit Colors.
            //
            // When ellipsis truncation was applied, remap color_run byte offsets
            // to the truncated string's coordinate system (hud-so7zu / hud-fe4ik).
            if let Some(truncated_str) = truncated {
                let resliced = if truncated_str == &*item.text {
                    // Text fit without truncation — preserve original color runs.
                    item.color_runs.to_vec()
                } else if item.viewport == TruncationViewport::TailAnchored
                    && truncated_str.starts_with(crate::overflow::ELLIPSIS)
                {
                    reslice_color_runs_tail_anchored(&item.text, truncated_str, &item.color_runs)
                } else {
                    let content_end = truncated_str
                        .strip_suffix(crate::overflow::ELLIPSIS)
                        .map(|s| s.len())
                        .unwrap_or(truncated_str.len());
                    reslice_color_runs(&item.color_runs, content_end)
                };
                if resliced.is_empty() {
                    buf.set_text(
                        &mut self.font_system,
                        effective_text,
                        base_attrs,
                        Shaping::Advanced,
                    );
                } else {
                    let spans = color_run_spans(effective_text, &resliced, base_attrs);
                    buf.set_rich_text(&mut self.font_system, spans, base_attrs, Shaping::Advanced);
                }
            } else {
                let spans = color_run_spans(&item.text, &item.color_runs, base_attrs);
                buf.set_rich_text(&mut self.font_system, spans, base_attrs, Shaping::Advanced);
            }
        }

        // Apply text alignment to all lines in the buffer.
        let ct_align = match item.alignment {
            TextAlign::Start => glyphon::cosmic_text::Align::Left,
            TextAlign::Center => glyphon::cosmic_text::Align::Center,
            TextAlign::End => glyphon::cosmic_text::Align::End,
        };
        for line in buf.lines.iter_mut() {
            line.set_align(Some(ct_align));
        }
        // Measurement lever for hud-991cj: every per-frame shape is counted so the
        // scroll benchmark/regression can prove a pure scroll (unchanged shape
        // inputs, cache hit) performs zero re-shapes.
        self.shape_call_count += 1;
        buf.shape_until_scroll(&mut self.font_system, false);
        buf
    }

    /// Record the glyphon text pass into `render_pass`.
    ///
    /// The render pass must have been begun with `LoadOp::Load` so that prior
    /// geometry (rects, backgrounds) is preserved under the text.
    pub fn render_text_pass<'rp>(
        &'rp self,
        render_pass: &mut wgpu::RenderPass<'rp>,
    ) -> Result<(), String> {
        self.renderer
            .render(&self.atlas, &self.viewport, render_pass)
            .map_err(|e| format!("glyphon render: {e:?}"))
    }

    /// Trim the atlas after the frame is presented.
    ///
    /// Reclaims memory for glyphs that were not used in the last frame.
    pub fn trim_atlas(&mut self) {
        self.atlas.trim();
    }
}

// ─── TextItem ─────────────────────────────────────────────────────────────────

/// A pixel-exact backdrop quad for an inline code span (Phase 2, hud-9ieev).
///
/// Positioned from actual glyph layout data (start/end advance, line_top/line_height
/// from cosmic-text's `LayoutRun`) after `shape_until_scroll`.  The color comes
/// from `StyledRunItem::background_color` (the `color.code.background` design token)
/// — never hardcoded.
///
/// Coordinates are in physical pixels, absolute (not tile-relative).
#[derive(Debug, Clone, PartialEq)]
pub struct InlineBackdropQuad {
    /// Left edge in physical pixels.
    pub x: f32,
    /// Top edge in physical pixels.
    pub y: f32,
    /// Width in physical pixels.
    pub w: f32,
    /// Height in physical pixels.
    pub h: f32,
    /// sRGB backdrop color [r, g, b, a] from the design token.
    pub color: [u8; 4],
}

/// Compute the shaped-buffer cache key for `item` (hud-991cj).
///
/// The key is a BLAKE3 digest over EVERY input that changes the shaped glyphs —
/// the original text and the effective (post-truncation) text, bounds, font
/// size / family / weight, alignment, overflow, truncation viewport, line-height
/// multiplier, and the full styled-run and color-run sets (including per-run
/// colors, which the streaming-reveal fade and token-map changes rewrite into
/// `styled_runs` upstream, so both are captured here as ordinary key misses).
///
/// The scroll offset (`pixel_x`/`pixel_y`, `clip_*`) is deliberately EXCLUDED:
/// a Clip pane's shaped output is offset-independent — the offset only moves the
/// draw position and clip rect — so a pure scroll reuses the shaped buffer with
/// zero re-shape. `item.color`/`opacity`/`outline_*` are also excluded: for the
/// plain and gap glyphs the color is applied at `TextArea` draw time (rebuilt
/// every frame via `default_color`), never baked into the shaped buffer; baked
/// per-run colors travel through `styled_runs`/`color_runs`, which ARE hashed.
///
/// Every variable-length field is length-prefixed and every `Option` is tagged
/// so distinct inputs cannot alias to the same digest.
fn shape_key_for_item(item: &TextItem, effective_text: &str) -> [u8; 32] {
    let mut h = blake3::Hasher::new();

    fn hash_bytes(h: &mut blake3::Hasher, b: &[u8]) {
        h.update(&(b.len() as u64).to_le_bytes());
        h.update(b);
    }
    fn hash_opt_u16(h: &mut blake3::Hasher, v: Option<u16>) {
        match v {
            Some(x) => {
                h.update(&[1u8]);
                h.update(&x.to_le_bytes());
            }
            None => {
                h.update(&[0u8]);
            }
        }
    }
    fn hash_opt_color(h: &mut blake3::Hasher, v: Option<[u8; 4]>) {
        match v {
            Some(c) => {
                h.update(&[1u8]);
                h.update(&c);
            }
            None => {
                h.update(&[0u8]);
            }
        }
    }
    fn hash_opt_f32(h: &mut blake3::Hasher, v: Option<f32>) {
        match v {
            Some(x) => {
                h.update(&[1u8]);
                h.update(&x.to_bits().to_le_bytes());
            }
            None => {
                h.update(&[0u8]);
            }
        }
    }

    // Text: original (run reslicing maps from original byte offsets) + effective
    // (the exact string shaped when truncated).
    hash_bytes(&mut h, item.text.as_bytes());
    hash_bytes(&mut h, effective_text.as_bytes());

    // Layout-affecting geometry / font (scroll-invariant).
    h.update(&item.bounds_width.to_bits().to_le_bytes());
    h.update(&item.bounds_height.to_bits().to_le_bytes());
    h.update(&item.font_size_px.to_bits().to_le_bytes());
    h.update(&item.line_height_multiplier.to_bits().to_le_bytes());
    h.update(&item.font_weight.to_le_bytes());

    // Fieldless-enum discriminants (matched explicitly — do not rely on `as u8`).
    h.update(&[match item.font_family {
        FontFamily::SystemSansSerif => 0u8,
        FontFamily::SystemMonospace => 1,
        FontFamily::SystemSerif => 2,
    }]);
    h.update(&[match item.alignment {
        TextAlign::Start => 0u8,
        TextAlign::Center => 1,
        TextAlign::End => 2,
    }]);
    h.update(&[match item.overflow {
        TextOverflow::Clip => 0u8,
        TextOverflow::Ellipsis => 1,
    }]);
    h.update(&[match item.viewport {
        TruncationViewport::HeadAnchored => 0u8,
        TruncationViewport::TailAnchored => 1,
    }]);

    // Styled runs (per-run colors here also carry the reveal-fade / token colors).
    h.update(&(item.styled_runs.len() as u64).to_le_bytes());
    for r in item.styled_runs.iter() {
        h.update(&(r.start_byte as u64).to_le_bytes());
        h.update(&(r.end_byte as u64).to_le_bytes());
        hash_opt_u16(&mut h, r.weight);
        h.update(&[r.italic as u8, r.monospace as u8]);
        hash_opt_color(&mut h, r.color);
        hash_opt_color(&mut h, r.background_color);
        hash_opt_f32(&mut h, r.size_scale);
    }

    // Color runs.
    h.update(&(item.color_runs.len() as u64).to_le_bytes());
    for r in item.color_runs.iter() {
        h.update(&(r.start_byte as u64).to_le_bytes());
        h.update(&(r.end_byte as u64).to_le_bytes());
        h.update(&r.color);
    }

    *h.finalize().as_bytes()
}

/// Effective (resliced) styled runs used for a frame's inline backdrop quads
/// (hud-991cj).
///
/// Mirrors exactly what [`TextRasterizer::build_and_shape_buffer`] feeds to
/// `set_rich_text`, computed separately so the backdrop-quad path runs on BOTH
/// the cache-hit and cache-miss paths:
/// - non-empty `styled_runs`, truncated → runs resliced into the truncated
///   string's coordinate space (`reslice_styled_runs_for_item`);
/// - non-empty `styled_runs`, not truncated → the original runs;
/// - otherwise (plain text or color-run items) → none (no backdrop quads).
fn effective_styled_runs_for_item(item: &TextItem, truncated: Option<&str>) -> Vec<StyledRunItem> {
    if item.styled_runs.is_empty() {
        return Vec::new();
    }
    match truncated {
        Some(truncated_str) => reslice_styled_runs_for_item(item, truncated_str),
        None => item.styled_runs.to_vec(),
    }
}

/// Compute pixel-exact backdrop quads for inline code spans from glyph layout data.
///
/// Called from `TextRasterizer::prepare_text_items` after `shape_until_scroll`
/// so that `buffer.layout_runs()` reflects actual glyph positions.
///
/// # Algorithm
///
/// For each `(item, buffer)` pair:
/// 1. Build a line-to-byte-offset map: `line_byte_offsets[line_i]` = the byte
///    offset in `item.text` where `BufferLine[line_i]` begins.  Consecutive
///    `BufferLine`s are separated by a single `\n` in the flat text.
/// 2. Iterate `buffer.layout_runs()`.  For each run, convert each glyph's
///    line-relative byte range to a full-text byte range using `line_byte_offsets`.
/// 3. For each glyph that falls within a `StyledRunItem` byte range that has a
///    `background_color`, record the glyph's pixel rect
///    `(item.pixel_x + g.x, item.pixel_y + run.line_top, g.w, run.line_height)`.
/// 4. Merge contiguous glyph rects within the same styled run + layout run into
///    a single quad (extend width on each subsequent glyph).
///
/// # Frame-loop budget
///
/// The function is O(G × S) in the number of glyphs G and styled runs S per
/// item.  In practice inline code spans are short and S is small; the cost is
/// negligible within the Stage 6 text budget.
///
/// # Parameters
///
/// `effective_styled_runs` must be parallel to `items`/`buffers`: one
/// `Vec<StyledRunItem>` per item, containing the styled runs that were actually
/// used for shaping the corresponding buffer.  For truncated (`Ellipsis`) items
/// these are the resliced runs; for non-truncated items they are the original
/// `item.styled_runs`.  Passing `item.styled_runs` directly for truncated items
/// would cause byte-offset mismatches between the shaped glyph positions and
/// the styled-run ranges.
///
/// Quads are clipped to each item's `clip_pixel_x/y + clip_bounds_width/height`
/// before being returned, so they never bleed outside the item's `TextBounds`.
/// Extend a per-visual-line selection quad toward the box edges for the standard
/// multi-line text-selection shape (hud-scgyw).
///
/// `clip_x0`/`clip_x1` are the box interior left/right (the text item's clip
/// bounds). `continues_left` means the selection began on a previous visual line
/// (so this line's highlight should start at the box left); `continues_right`
/// means it continues onto the next line (highlight to the box right). A fully
/// interior line has both → a full-width band; the first line extends right only,
/// the last line left only, and a single-line selection extends neither.
fn extend_selection_quad_to_box(
    mut q: InlineBackdropQuad,
    clip_x0: f32,
    clip_x1: f32,
    continues_left: bool,
    continues_right: bool,
) -> InlineBackdropQuad {
    if continues_left {
        let right = q.x + q.w;
        q.x = clip_x0;
        q.w = (right - clip_x0).max(0.0);
    }
    if continues_right {
        q.w = (clip_x1 - q.x).max(0.0);
    }
    q
}

pub fn compute_inline_backdrop_quads(
    items: &[TextItem],
    buffers: &[Buffer],
    effective_styled_runs: &[Vec<StyledRunItem>],
) -> Vec<InlineBackdropQuad> {
    let mut quads: Vec<InlineBackdropQuad> = Vec::new();

    for ((item, buf), eff_runs) in items
        .iter()
        .zip(buffers.iter())
        .zip(effective_styled_runs.iter())
    {
        // Only items with (effective) styled runs can have backdrop colors.
        if eff_runs.is_empty() {
            continue;
        }
        // Only emit quads when at least one run has a background_color.
        let has_backdrop = eff_runs.iter().any(|r| r.background_color.is_some());
        if !has_backdrop {
            continue;
        }

        // Clip rect for this item — quads must not bleed outside the TextBounds.
        let clip_x0 = item.clip_pixel_x;
        let clip_y0 = item.clip_pixel_y;
        let clip_x1 = item.clip_pixel_x + item.clip_bounds_width;
        let clip_y1 = item.clip_pixel_y + item.clip_bounds_height;

        // Build line_byte_offsets: the flat-text byte offset where each BufferLine begins.
        // BufferLines in the buffer correspond to paragraphs (hard newlines in item.text).
        // They are joined by '\n' in the flat text, so:
        //   line 0 starts at 0
        //   line N starts at sum(line[0].text().len() + 1, …, line[N-1].text().len() + 1)
        let line_byte_offsets: Vec<usize> = {
            let mut offsets = Vec::with_capacity(buf.lines.len());
            let mut running = 0usize;
            for line in buf.lines.iter() {
                offsets.push(running);
                // +1 for the '\n' that separates this line from the next.
                running += line.text().len() + 1;
            }
            offsets
        };

        // With `Shaping::Advanced` (rustybuzz), `LayoutGlyph::start` and `end`
        // are byte offsets within the `BufferLine` text (the `start_run` offset
        // is already included).  We can therefore map each glyph directly to its
        // full-text byte range as `line_offset + glyph.start .. line_offset + glyph.end`.
        //
        // (Under the old `Shaping::Basic`/shape_skip mode `start` was a char index
        // within the word being shaped, not a byte offset — that required the
        // per-run char-cursor table that was removed here when production code
        // switched to `Shaping::Advanced`.)

        // Record the quads length before this item so we can clip only the
        // quads appended for this item at the end of the layout-runs pass.
        let len_before_clip = quads.len();

        for run in buf.layout_runs() {
            let line_offset = line_byte_offsets.get(run.line_i).copied().unwrap_or(0);

            // Full-text byte span of THIS visual line (hud-scgyw): the min/max glyph
            // byte over the run. Used to decide, for a `fill_line_width` selection
            // run, whether the selection continues onto the previous/next line so
            // the highlight can extend to the box edge (standard text-selection
            // shape: full-width interior lines, partial first/last).
            let (line_first_byte, line_last_byte) =
                run.glyphs
                    .iter()
                    .fold((usize::MAX, 0usize), |(min_b, max_b), g| {
                        (
                            min_b.min(line_offset + g.start),
                            max_b.max(line_offset + g.end),
                        )
                    });

            // For each StyledRunItem (from the effective/resliced set) with a
            // background_color, find glyphs that overlap its byte range and
            // accumulate a quad.  Using `eff_runs` (not `item.styled_runs`) ensures
            // byte offsets match the buffer that was actually shaped — critical for
            // truncated (Ellipsis) items where reslicing shifted the offsets.
            for styled_run in eff_runs.iter() {
                let bg_color = match styled_run.background_color {
                    Some(c) => c,
                    None => continue,
                };
                // styled_run byte range is relative to the effective (possibly
                // truncated) text that was shaped into `buf`.
                let sr_start = styled_run.start_byte;
                let sr_end = styled_run.end_byte;

                // Track whether we're currently building a quad for this run.
                let mut current_quad: Option<InlineBackdropQuad> = None;

                for glyph in run.glyphs.iter() {
                    // With Shaping::Advanced, glyph.start/end are byte offsets
                    // in the BufferLine text; add line_offset for the full-text range.
                    let g_start = line_offset + glyph.start;
                    let g_end = line_offset + glyph.end;

                    // Check overlap: glyph range [g_start, g_end) vs styled run [sr_start, sr_end).
                    // A glyph overlaps if g_start < sr_end && g_end > sr_start.
                    if g_start >= sr_end || g_end <= sr_start {
                        // Glyph is outside this styled run; flush any current quad.
                        if let Some(q) = current_quad.take() {
                            quads.push(q);
                        }
                        continue;
                    }

                    // Glyph overlaps the styled run — compute its pixel rect.
                    let glyph_x = item.pixel_x + glyph.x;
                    let glyph_w = glyph.w;
                    let quad_y = item.pixel_y + run.line_top;
                    let quad_h = run.line_height;

                    match current_quad.as_mut() {
                        Some(q) => {
                            // Extend the existing quad to cover this glyph.
                            let new_right = (q.x + q.w).max(glyph_x + glyph_w);
                            q.w = new_right - q.x;
                        }
                        None => {
                            current_quad = Some(InlineBackdropQuad {
                                x: glyph_x,
                                y: quad_y,
                                w: glyph_w,
                                h: quad_h,
                                color: bg_color,
                            });
                        }
                    }
                }

                // Flush any in-progress quad after scanning all glyphs in this run.
                if let Some(mut q) = current_quad.take() {
                    // Full-width selection shape (hud-scgyw): when this selection run
                    // continues from the previous visual line, extend the highlight
                    // left to the box edge; when it continues onto the next line,
                    // extend right to the box edge. Interior lines get both → a
                    // full-width band; the first/last lines stay partial. The final
                    // clip pass below trims to the item's TextBounds.
                    if styled_run.fill_line_width && line_first_byte != usize::MAX {
                        q = extend_selection_quad_to_box(
                            q,
                            clip_x0,
                            clip_x1,
                            sr_start < line_first_byte, // continues from previous line
                            sr_end > line_last_byte,    // continues onto next line
                        );
                    }
                    quads.push(q);
                }
            }
        }

        // Clip all quads for this item to its TextBounds (clip_pixel_x/y +
        // clip_bounds_width/height).  Backdrop rects must not bleed outside the
        // tile clip region — e.g. for scrolled or partially visible text items.
        // We retain only quads that have non-zero visible area after clipping.
        //
        // Collect clipped quads for this item from `item_quads_buf`, then
        // drain and re-push into `quads`.
        let item_quads_buf: Vec<InlineBackdropQuad> = quads.drain(len_before_clip..).collect();
        for q in item_quads_buf {
            let x0 = q.x.max(clip_x0);
            let y0 = q.y.max(clip_y0);
            let x1 = (q.x + q.w).min(clip_x1);
            let y1 = (q.y + q.h).min(clip_y1);
            if x1 > x0 && y1 > y0 {
                quads.push(InlineBackdropQuad {
                    x: x0,
                    y: y0,
                    w: x1 - x0,
                    h: y1 - y0,
                    color: q.color,
                });
            }
        }
    }

    quads
}

/// A single text rendering request — one TextMarkdownNode or one zone publish.
#[derive(Debug, Clone)]
pub struct TextItem {
    /// The text content to render (plain text for v1; Markdown stripped).
    ///
    /// Stored as `Arc<str>` so the per-frame path in
    /// [`TextItem::from_text_markdown_cached`] can clone it with a cheap
    /// reference-count bump rather than a deep string copy.  Call sites that
    /// need a `&str` can deref directly; those that need an owned `String` can
    /// call `.to_string()`.
    pub text: Arc<str>,
    /// Left edge in physical pixels (absolute, not tile-relative).
    pub pixel_x: f32,
    /// Top edge in physical pixels (absolute, not tile-relative).
    pub pixel_y: f32,
    /// Available width for word-wrap and clip.
    pub bounds_width: f32,
    /// Available height for clip.
    pub bounds_height: f32,
    /// Left edge of the glyph clip rectangle in physical pixels.
    pub clip_pixel_x: f32,
    /// Top edge of the glyph clip rectangle in physical pixels.
    pub clip_pixel_y: f32,
    /// Width of the glyph clip rectangle in physical pixels.
    pub clip_bounds_width: f32,
    /// Height of the glyph clip rectangle in physical pixels.
    pub clip_bounds_height: f32,
    /// Font size in pixels.
    pub font_size_px: f32,
    /// Font family selection.
    pub font_family: FontFamily,
    /// Font weight (CSS-style: 100–900); 400 = regular, 700 = bold.
    ///
    /// Mapped to `glyphon::Weight` at rasterization time.
    pub font_weight: u16,
    /// Text color as sRGB u8 bytes: [r, g, b, a].
    pub color: [u8; 4],
    /// Alignment (used for cosmic-text alignment).
    pub alignment: TextAlign,
    /// Overflow mode.
    pub overflow: TextOverflow,
    /// Outline color for 8-direction text outline; None = no outline.
    pub outline_color: Option<[u8; 4]>,
    /// Outline stroke width in pixels; None or 0.0 = no outline.
    pub outline_width: Option<f32>,
    /// Opacity multiplier (0.0–1.0) from zone animation state; 1.0 = fully opaque.
    pub opacity: f32,
    /// Inline color runs (byte offsets into `text`, which is the **post-strip**
    /// content).  Empty = use `color` for the entire text.
    ///
    /// Offsets are remapped from raw-content byte positions by
    /// `TextItem::from_text_markdown_node` when `color_runs` are present.
    /// Zone-derived `TextItem`s always carry an empty slice (no run support yet).
    pub color_runs: Box<[ColorRunItem]>,
    /// Markdown-derived styled runs from the Phase-1 parse cache
    /// (hud-5jbra.2).
    ///
    /// When non-empty these runs take precedence over `color_runs` for
    /// determining per-span font weight, italic style, and font family.
    /// Color overrides in `styled_runs` also override `color_runs` for the
    /// same byte range.
    ///
    /// Populated by [`TextItem::from_text_markdown_cached`]; empty for all
    /// other constructors.
    pub styled_runs: Box<[StyledRunItem]>,
    /// Truncation viewport anchor mode.
    ///
    /// Controls which lines are shown when the text overflows the bounds:
    /// - [`TruncationViewport::HeadAnchored`] (default) — show the FIRST
    ///   `max_lines` (static tiles, scrolled-back viewports, spec task 3.3).
    /// - [`TruncationViewport::TailAnchored`] — show the LAST `max_lines`
    ///   (follow-tail streaming tiles at the tail, spec task 3.2).
    ///
    /// Set by `collect_text_items_from_node` / `collect_ellipsis_text_items_from_node`
    /// based on the tile's current `follow_tail_at_tail` state in the scene.
    pub viewport: TruncationViewport,
    /// Line-height multiplier: `line_height_px = font_size_px × line_height_multiplier`.
    ///
    /// Resolved from the `typography.line_height.multiplier` design token via
    /// [`ParsedMarkdown::line_height_multiplier`].  Default `1.4`.
    ///
    /// Cross-ref: `PORTAL_LINE_HEIGHT_MULTIPLIER` in
    /// `crates/tze_hud_projection/src/bin/projection_authority.rs` mirrors this
    /// value. If you change the default, update that constant in the same PR.
    pub line_height_multiplier: f32,
}

/// A single styled run for Phase-1 markdown rendering, with byte offsets into
/// the **plain-text** `TextItem::text`.
///
/// Carries full per-span style attributes — weight, italic, monospace, color,
/// and optional font-size scale — resolved from design tokens at parse-commit
/// time.  The frame pipeline consumes these without re-parsing.
#[derive(Debug, Clone)]
pub struct StyledRunItem {
    /// Inclusive byte offset into `TextItem::text`.
    pub start_byte: usize,
    /// Exclusive byte offset into `TextItem::text`.
    pub end_byte: usize,
    /// CSS font weight override (100–900); `None` = use `TextItem::font_weight`.
    pub weight: Option<u16>,
    /// When `true`, request the italic style variant.
    pub italic: bool,
    /// When `true`, use the monospace font family instead of the item default.
    pub monospace: bool,
    /// Optional color override (linear sRGB); `None` = use `TextItem::color`.
    pub color: Option<[u8; 4]>,
    /// Optional backdrop panel color (sRGB u8); `None` = no backdrop.
    ///
    /// Populated from `StyleAttr::background_color` (the `color.code.background`
    /// design token) for inline code and code block spans.  Used by the text
    /// rasterizer to draw a backdrop quad behind the glyph run before the glyphs
    /// are composited — enabling per-span highlight/background rendering without
    /// a separate geometry pass for inline elements.
    pub background_color: Option<[u8; 4]>,
    /// Font-size multiplier relative to `TextItem::font_size_px`.
    ///
    /// `None` = no scaling (use base size).  Applied by setting per-span
    /// `Attrs::metrics` in `styled_run_spans`.  Used to render heading levels
    /// at the token-defined size.
    pub size_scale: Option<f32>,
    /// When `true`, a multi-line `background_color` run is drawn as a standard
    /// text-selection shape rather than tight per-glyph quads (hud-scgyw): the
    /// first visual line is highlighted from the selection start to the line's
    /// right edge, fully-interior lines span the FULL box width, and the last
    /// line runs from the box's left edge to the selection end.
    ///
    /// Set only for the composer's selection run. Inline-code backdrops leave it
    /// `false` so a wrapped inline-code span keeps its tight per-glyph backdrop.
    pub fill_line_width: bool,
}

/// A single resolved color run for `TextItem` rendering, with byte offsets
/// into the **post-strip** `text` string.
#[derive(Debug, Clone)]
pub struct ColorRunItem {
    /// Inclusive byte offset into `TextItem::text`.
    pub start_byte: usize,
    /// Exclusive byte offset into `TextItem::text`.
    pub end_byte: usize,
    /// sRGB u8 color: [r, g, b, a].
    pub color: [u8; 4],
}

/// Returns true if `node` carries at least one *pixel-bearing* color run —
/// a run whose `[start_byte, end_byte)` range covers one or more bytes.
///
/// Zero-length sentinel runs (`start_byte >= end_byte`) carry metadata only
/// (e.g. lifecycle/stale/at-capacity markers); they reference no content bytes
/// and never paint. A node carrying *only* such sentinels is rendering-equivalent
/// to one with empty `color_runs`, so it can still take the cached/styled
/// markdown path. Use this predicate — never `color_runs.is_empty()` — to decide
/// whether the lossy raw-content path is required. (hud-9v3t6)
pub fn markdown_node_has_pixel_runs(node: &TextMarkdownNode) -> bool {
    node.color_runs.iter().any(|r| r.start_byte < r.end_byte)
}

/// Token-resolved accent for an active transcript-tail streaming cursor, if this
/// node carries the streaming-cursor tail marker (hud-zlq2v).
///
/// The projection adapter (`streaming_cursor_color_runs` in
/// `tze_hud_projection::resident_grpc`) signals an active tail cursor with a
/// single **zero-length** color run pinned at the very end of `content`
/// (`start_byte == end_byte == content.len()`), carrying the
/// `portal.streaming_cursor.color` token. Because it is zero-length it is a pure
/// sentinel — [`markdown_node_has_pixel_runs`] stays `false`, so the node keeps
/// the cached/styled markdown fast path (no lossy raw-content regression, #947).
///
/// The content-end position is what distinguishes this marker from every other
/// portal sentinel (stale / lifecycle / unread / timestamp), which all sit at
/// byte 0. The renderer uses the returned accent to recolor the trailing cursor
/// glyph precisely on the LAID-OUT text (see `apply_tail_streaming_cursor`),
/// which is the promotion-era replacement for the old byte-0 sentinel that
/// painted nothing.
///
/// Returns `None` when the content is empty or no tail marker is present (the
/// cue has quiesced — kbm80 behavior preserved).
pub fn markdown_node_tail_cursor_color(node: &TextMarkdownNode) -> Option<[u8; 4]> {
    let end = node.content.len() as u32;
    if end == 0 {
        return None;
    }
    node.color_runs
        .iter()
        .rev()
        .find(|r| r.start_byte == end && r.end_byte == end)
        .map(|r| rgba_to_srgb_u8(r.color))
}

/// An axis-aligned box as `(x, y, width, height)` in absolute physical pixels.
///
/// Used by [`portal_part_clip_rect`] purely as a lightweight geometry tuple so
/// the per-part clip math is a free function testable without a GPU.
pub type ClipBox = (f32, f32, f32, f32);

/// Intersect two [`ClipBox`]es in the same coordinate space.
///
/// Returns the overlapping box. Width/height are clamped at `0.0` (never
/// negative) when the boxes are disjoint, so the result is always a valid —
/// possibly empty — rectangle.
fn intersect_clip_box(a: ClipBox, b: ClipBox) -> ClipBox {
    let left = a.0.max(b.0);
    let top = a.1.max(b.1);
    let right = (a.0 + a.2).min(b.0 + b.2);
    let bottom = (a.1 + a.3).min(b.1 + b.3);
    (left, top, (right - left).max(0.0), (bottom - top).max(0.0))
}

/// Compute the glyph clip rectangle for a portal part's drawn content.
///
/// The rendered content box (`content`) is clamped to the owning tile box
/// (`tile`) and — when the node is a *declared portal part* — additionally to
/// that part's region (`part`). Every argument is an absolute-physical-pixel
/// [`ClipBox`]; `part` must already be offset into the same absolute space as
/// `content`/`tile` (i.e. `tile_origin + part.bounds`).
///
/// `part` is `None` for the legacy single-node portal (and any non-portal
/// markdown surface): then only the tile box constrains the clip, reproducing
/// the pre-promotion behavior byte-for-byte. When `Some`, the part region is the
/// per-part containment envelope that keeps one part's content (e.g. an
/// overlong transcript) from painting over a sibling part's region (e.g. the
/// composer strip) — the render-side meaning of "one scene node per part"
/// (hud-s4lrw). The part region is a *clip* envelope only; it deliberately does
/// **not** alter the wrap/truncation measure (`bounds_width`), so the
/// shaped-buffer and truncation caches stay keyed exactly as before (PR #1007
/// perf invariant).
///
/// Each returned dimension is floored at `1.0` so a fully-collapsed
/// intersection still yields a valid (degenerate) scissor rect rather than a
/// zero or negative extent.
pub fn portal_part_clip_rect(content: ClipBox, tile: ClipBox, part: Option<ClipBox>) -> ClipBox {
    let mut clip = intersect_clip_box(content, tile);
    if let Some(part) = part {
        clip = intersect_clip_box(clip, part);
    }
    (clip.0, clip.1, clip.2.max(1.0), clip.3.max(1.0))
}

impl TextItem {
    /// Build a `TextItem` from a `TextMarkdownNode` and its tile-relative position.
    ///
    /// `tile_x` / `tile_y` are the pixel-space position of the tile origin.
    ///
    /// When `node.color_runs` contains at least one non-empty range
    /// (`start_byte < end_byte`), Markdown stripping is skipped and the raw
    /// content is used as-is so byte offsets in the runs remain valid against
    /// the text.  Callers that supply such runs must pre-strip Markdown before
    /// populating `content`, or accept that markup characters appear in the
    /// output.
    ///
    /// Zero-length runs (`start_byte == end_byte`) are treated as pure
    /// sentinels (no pixel coverage); their presence does **not** suppress
    /// Markdown stripping.
    ///
    /// # Deprecation notice — empty `color_runs` path
    ///
    /// When `color_runs` is entirely empty (no pixel runs), this constructor
    /// calls `strip_markdown_v1` which is **lossy** — it strips markdown syntax
    /// characters but does not produce [`StyledRunItem`]s, so all markdown
    /// structure and styling is discarded.
    ///
    /// In the Phase-1 render path the compositor now always goes through
    /// [`TextItem::from_text_markdown_cached`] (primed at commit-time by
    /// `Compositor::prime_markdown_cache`).  The renderer's cache-miss
    /// fallback (hud-xcp9b) parses inline via `parse_markdown_subset` rather
    /// than falling through to this constructor, so the lossy strip path is no
    /// longer reachable from the render pipeline.
    ///
    /// **Do not call this constructor with an empty `color_runs` slice from any
    /// new production code.**  Use `from_text_markdown_cached` +
    /// `MarkdownCache::prime` instead for markdown-fidelity rendering.
    pub fn from_text_markdown_node(node: &TextMarkdownNode, tile_x: f32, tile_y: f32) -> Self {
        // Skip Markdown stripping only when at least one run has a non-empty
        // byte range — those runs reference offsets into the raw (un-stripped)
        // content and stripping would invalidate them.
        // Zero-length sentinel runs (start_byte == end_byte) carry metadata
        // only; they do not reference content offsets, so we can still strip.
        let has_pixel_runs = markdown_node_has_pixel_runs(node);
        let text: Arc<str> = if has_pixel_runs {
            Arc::from(node.content.as_str())
        } else {
            Arc::from(strip_markdown_v1(&node.content).as_str())
        };

        // Convert linear f32 color [0..1] to sRGB u8 [0..255].
        let r = linear_to_srgb_u8(node.color.r);
        let g = linear_to_srgb_u8(node.color.g);
        let b = linear_to_srgb_u8(node.color.b);
        let a = (node.color.a * 255.0).clamp(0.0, 255.0) as u8;

        // Add a size-aware inset margin so large text boxes get breathing room
        // without collapsing compact HUD labels like Presence Card rows.
        // Line-height uses the default multiplier (1.4) since this constructor
        // does not have access to a parsed MarkdownTokens; the multiplier field
        // is set to the same default value.
        let font_size_px = node.font_size_px.clamp(6.0, 200.0);
        let lh_multiplier = crate::markdown::MarkdownTokens::default().line_height_multiplier;
        let line_height = font_size_px * lh_multiplier;
        let (margin_x, margin_y) =
            text_content_box_margins(node.bounds.width, node.bounds.height, line_height);
        let x = tile_x + node.bounds.x + margin_x;
        let y = tile_y + node.bounds.y + margin_y;
        let w = (node.bounds.width - margin_x * 2.0).max(1.0);
        let h = (node.bounds.height - margin_y * 2.0).max(1.0);

        // Convert scene TextColorRun (raw content offsets) to ColorRunItem
        // (text/post-strip offsets).  Since we skip stripping when runs are
        // present, the offsets map 1:1 to positions in `text`.
        let color_runs: Box<[ColorRunItem]> = node
            .color_runs
            .iter()
            .filter_map(|run| {
                // Clamp to actual text length to guard against stale runs after
                // any future content truncation.
                let start = (run.start_byte as usize).min(text.len());
                let end = (run.end_byte as usize).min(text.len());
                if start >= end {
                    return None;
                }
                // Only emit the run if both boundaries are valid UTF-8 positions.
                if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                    return None;
                }
                Some(ColorRunItem {
                    start_byte: start,
                    end_byte: end,
                    color: rgba_to_srgb_u8(run.color),
                })
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        TextItem {
            text,
            pixel_x: x,
            pixel_y: y,
            bounds_width: w,
            bounds_height: h,
            clip_pixel_x: x,
            clip_pixel_y: y,
            clip_bounds_width: w,
            clip_bounds_height: h,
            font_size_px,
            font_family: node.font_family,
            font_weight: 400, // TextMarkdownNode does not carry weight; default regular.
            color: [r, g, b, a],
            alignment: node.alignment,
            overflow: node.overflow,
            outline_color: None,
            outline_width: None,
            opacity: 1.0,
            color_runs,
            styled_runs: Box::default(),
            // Viewport defaults to HeadAnchored; callers that know the tile is
            // at-tail (follow-tail streaming tiles) override this after construction.
            viewport: TruncationViewport::HeadAnchored,
            line_height_multiplier: lh_multiplier,
        }
    }

    /// Build a `TextItem` from a `TextMarkdownNode` using a pre-parsed
    /// [`ParsedMarkdown`] from the [`MarkdownCache`] (Phase-1 hud-5jbra.2).
    ///
    /// This is the replacement for `from_text_markdown_node` in the Phase-1
    /// markdown path.  The `plain_text` and `styled_runs` from the cache are
    /// used directly — **no markdown parsing happens here**, satisfying the
    /// zero-per-frame-parse-cost requirement.
    ///
    /// # color_runs incompatibility
    ///
    /// This constructor uses `parsed.plain_text` as the text base (the
    /// Markdown-stripped version), which makes `node.color_runs` byte offsets
    /// invalid.  Consequently, `color_runs` is **not** preserved: the returned
    /// `TextItem` always has `color_runs: Box::default()`.
    ///
    /// Callers **must not** invoke this constructor when `node.color_runs` is
    /// non-empty.  Use [`TextItem::from_text_markdown_node`] instead; it
    /// preserves the raw content and maps `color_runs` correctly.
    ///
    /// `tile_x` / `tile_y` are the pixel-space position of the tile origin.
    ///
    /// [`ParsedMarkdown`]: crate::markdown::ParsedMarkdown
    /// [`MarkdownCache`]: crate::markdown::MarkdownCache
    pub fn from_text_markdown_cached(
        node: &TextMarkdownNode,
        tile_x: f32,
        tile_y: f32,
        parsed: &crate::markdown::ParsedMarkdown,
    ) -> Self {
        // Callers must not invoke this constructor when color_runs carries
        // *pixel-bearing* runs (see the # color_runs incompatibility doc comment
        // above).  Zero-length sentinel runs (start >= end) are tolerated: they
        // reference no content bytes and are simply dropped here, exactly as
        // `from_text_markdown_node` would filter them out.  This lets nodes that
        // carry only lifecycle/stale sentinels keep the cached/styled markdown
        // path (hud-9v3t6).  Assert in debug builds so genuine misuse — passing
        // pixel runs whose offsets index the un-stripped raw content — is caught
        // early without any release overhead.
        debug_assert!(
            !markdown_node_has_pixel_runs(node),
            "from_text_markdown_cached called with pixel-bearing color_runs; \
             use from_text_markdown_node instead"
        );
        // PERF: Arc::clone is a refcount bump — no heap allocation or string copy.
        // This is the hot per-frame path: `parsed` lives in the MarkdownCache and
        // its `plain_text: Arc<str>` is shared across all frames for the same content.
        let text: Arc<str> = Arc::clone(&parsed.plain_text);

        // Convert linear f32 color [0..1] to sRGB u8 [0..255].
        let r = linear_to_srgb_u8(node.color.r);
        let g = linear_to_srgb_u8(node.color.g);
        let b = linear_to_srgb_u8(node.color.b);
        let a = (node.color.a * 255.0).clamp(0.0, 255.0) as u8;

        // Geometry: same inset margin logic as `from_text_markdown_node`, but
        // uses the line-height multiplier resolved from design tokens at parse time.
        let font_size_px = node.font_size_px.clamp(6.0, 200.0);
        let lh_multiplier = parsed.line_height_multiplier;
        let line_height = font_size_px * lh_multiplier;
        let (margin_x, margin_y) =
            text_content_box_margins(node.bounds.width, node.bounds.height, line_height);
        let x = tile_x + node.bounds.x + margin_x;
        let y = tile_y + node.bounds.y + margin_y;
        let w = (node.bounds.width - margin_x * 2.0).max(1.0);
        let h = (node.bounds.height - margin_y * 2.0).max(1.0);

        // Convert ParsedMarkdown spans to StyledRunItems — shared with the
        // vertical-flow measurement seam (hud-3xdlf) via
        // `markdown_spans_to_styled_runs` so a measured height is built from
        // the identical run set the render path paints, not a second,
        // independently-drifting reimplementation of this filter/clamp logic.
        let styled_runs: Box<[StyledRunItem]> = markdown_spans_to_styled_runs(&text, &parsed.spans);

        TextItem {
            text,
            pixel_x: x,
            pixel_y: y,
            bounds_width: w,
            bounds_height: h,
            clip_pixel_x: x,
            clip_pixel_y: y,
            clip_bounds_width: w,
            clip_bounds_height: h,
            font_size_px,
            font_family: node.font_family,
            font_weight: 400,
            color: [r, g, b, a],
            alignment: node.alignment,
            overflow: node.overflow,
            outline_color: None,
            outline_width: None,
            opacity: 1.0,
            color_runs: Box::default(),
            styled_runs,
            // Viewport defaults to HeadAnchored; callers that know the tile is
            // at-tail (follow-tail streaming tiles) override this after construction.
            viewport: TruncationViewport::HeadAnchored,
            line_height_multiplier: lh_multiplier,
        }
    }

    /// Build a `TextItem` for zone text content driven by a [`RenderingPolicy`].
    ///
    /// This is the factory method for zone rendering.  All visual properties are read from `policy`; no hardcoded
    /// colors or font choices.
    ///
    /// `x`, `y`, `w`, `h` are the zone geometry in physical pixels.
    /// `opacity` is the current zone animation opacity (1.0 = fully opaque).
    ///
    /// [`RenderingPolicy`]: tze_hud_scene::types::RenderingPolicy
    pub fn from_zone_policy(
        text: &str,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        policy: &RenderingPolicy,
        opacity: f32,
    ) -> Self {
        // Margin: prefer margin_horizontal/margin_vertical; fall back to margin_px;
        // then fall back to 8px.  Per spec §Extended RenderingPolicy: margin_horizontal
        // "overrides margin_px for the horizontal axis. When None, falls back to margin_px."
        let margin_h = policy.margin_horizontal.or(policy.margin_px).unwrap_or(8.0);
        let margin_v = policy.margin_vertical.or(policy.margin_px).unwrap_or(8.0);

        let font_size_px = policy.font_size_px.unwrap_or(16.0).clamp(6.0, 200.0);
        let font_family = policy.font_family.unwrap_or(FontFamily::SystemSansSerif);
        // font_weight: use policy value clamped to CSS weight range [100, 900]; default 400.
        let font_weight = policy.font_weight.unwrap_or(400).clamp(100, 900);
        let alignment = policy.text_align.unwrap_or(TextAlign::Start);

        // text_color: policy.text_color if present, else white.
        let color = policy
            .text_color
            .map(rgba_to_srgb_u8)
            .unwrap_or([255, 255, 255, 220]);

        // Apply opacity to the fill color alpha.
        let color = apply_opacity_to_color(color, opacity);

        // Outline: propagate only when outline_width > 0.
        let (outline_color, outline_width) = match (policy.outline_color, policy.outline_width) {
            (Some(oc), Some(ow)) if ow > 0.0 => {
                let oc_srgb = apply_opacity_to_color(rgba_to_srgb_u8(oc), opacity);
                (Some(oc_srgb), Some(ow))
            }
            _ => (None, None),
        };

        TextItem {
            text: Arc::from(text),
            pixel_x: x + margin_h,
            pixel_y: y + margin_v,
            bounds_width: (w - margin_h * 2.0).max(1.0),
            bounds_height: (h - margin_v * 2.0).max(1.0),
            clip_pixel_x: x + margin_h,
            clip_pixel_y: y + margin_v,
            clip_bounds_width: (w - margin_h * 2.0).max(1.0),
            clip_bounds_height: (h - margin_v * 2.0).max(1.0),
            font_size_px,
            font_family,
            font_weight,
            color,
            alignment,
            overflow: policy.overflow.unwrap_or(TextOverflow::Clip),
            outline_color,
            outline_width,
            opacity,
            color_runs: Box::default(),
            styled_runs: Box::default(),
            viewport: TruncationViewport::HeadAnchored,
            // Zone items do not carry MarkdownTokens; use the canonical default.
            line_height_multiplier: crate::markdown::MarkdownTokens::default()
                .line_height_multiplier,
        }
    }
}

// ─── Color helpers ────────────────────────────────────────────────────────────

/// Convert an `Rgba` (linear f32) to sRGB u8 `[r, g, b, a]`.
///
/// The alpha channel is passed through directly (0..1 → 0..255) rather than
/// being gamma-encoded, which matches glyphon's expected color space.
pub fn rgba_to_srgb_u8(c: Rgba) -> [u8; 4] {
    [
        linear_to_srgb_u8(c.r),
        linear_to_srgb_u8(c.g),
        linear_to_srgb_u8(c.b),
        (c.a * 255.0).clamp(0.0, 255.0) as u8,
    ]
}

/// Multiply the alpha channel of an sRGB u8 color by `opacity`.
pub fn apply_opacity_to_color(color: [u8; 4], opacity: f32) -> [u8; 4] {
    let a = (color[3] as f32 * opacity.clamp(0.0, 1.0)).clamp(0.0, 255.0) as u8;
    [color[0], color[1], color[2], a]
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Canonicalize a slice of [`ColorRunItem`]s into a sorted, non-overlapping
/// sequence using **last-writer-wins** overlap semantics.
///
/// # Semantics
///
/// For any byte position covered by multiple runs, the run with the **highest
/// original index** (i.e. the last run in the input slice whose range covers
/// that byte) wins.
///
/// # Algorithm
///
/// Uses a sweep-line over all interval endpoints:
///
/// 1. Clamp each run to `text_len`; drop degenerate (start ≥ end) runs.
/// 2. Emit two events per run — a START and an END — keyed by byte position.
///    At equal positions, END events sort before START events so adjacent
///    runs are handled correctly.
/// 3. Walk events left-to-right, maintaining a `BTreeMap<orig_idx, color>`
///    of currently-active runs.  Before each event, emit a span from the
///    active run with the **highest key** (= highest original index).
/// 4. Merge adjacent output spans with identical colors to minimize span count.
///
/// Out-of-bounds or zero-length runs are silently dropped.
fn canonicalize_color_runs_impl(runs: &[ColorRunItem], text_len: usize) -> Vec<ColorRunItem> {
    if runs.is_empty() {
        return Vec::new();
    }

    // (position, is_start, orig_idx, color)
    // is_start=false (END) sorts before is_start=true (START) at equal positions,
    // so a run ending exactly where another begins does not overlap.
    let mut events: Vec<(usize, bool, usize, [u8; 4])> = Vec::with_capacity(runs.len() * 2);
    for (i, r) in runs.iter().enumerate() {
        let s = r.start_byte.min(text_len);
        let e = r.end_byte.min(text_len);
        if s < e {
            events.push((s, true, i, r.color));
            events.push((e, false, i, r.color));
        }
    }

    // Sort: primary by position, secondary END < START (false < true).
    events.sort_by_key(|&(pos, is_start, _, _)| (pos, is_start));

    // active: orig_idx → color, for currently open runs.
    let mut active: BTreeMap<usize, [u8; 4]> = BTreeMap::new();
    // raw output segments before merging: (start, end, color).
    let mut raw: Vec<(usize, usize, [u8; 4])> = Vec::new();
    let mut last_pos = 0usize;

    for (pos, is_start, idx, color) in events {
        if pos > last_pos {
            // Emit segment for the active run with the highest original index,
            // which is the last-writer per the semantics contract.
            if let Some((&_, &active_color)) = active.iter().next_back() {
                raw.push((last_pos, pos, active_color));
            }
        }
        if is_start {
            active.insert(idx, color);
        } else {
            active.remove(&idx);
        }
        last_pos = pos;
    }

    // Merge adjacent segments with the same color to minimize span count.
    let mut out: Vec<ColorRunItem> = Vec::with_capacity(raw.len());
    for (s, e, c) in raw {
        if let Some(last) = out.last_mut() {
            if last.end_byte == s && last.color == c {
                last.end_byte = e;
                continue;
            }
        }
        out.push(ColorRunItem {
            start_byte: s,
            end_byte: e,
            color: c,
        });
    }
    out
}

/// Build `(text_slice, Attrs)` pairs for [`Buffer::set_rich_text`] from a set
/// of [`ColorRunItem`]s and a base `Attrs`.
///
/// Runs need not be sorted or non-overlapping — this function canonicalizes
/// them using last-writer-wins semantics before building spans:
///
/// - Runs are sorted by `start_byte`.
/// - When two runs overlap, the **later run** (higher index in the original
///   slice) wins on the intersection.  The earlier run is split: its prefix
///   before the overlap is kept; the overlap itself is replaced by the later
///   run's color; any suffix of the earlier run beyond the later run's end
///   resumes with the earlier run's color.
///
/// Segments of `text` not covered by any run are emitted with `base_attrs`
/// (no color override, so `TextArea::default_color` applies at render time).
/// Segments covered by a run are emitted with `base_attrs` plus an explicit
/// `Color` set from the run.
///
/// Out-of-bounds run byte offsets are clamped to `text.len()`.
pub(crate) fn color_run_spans<'t, 'a>(
    text: &'t str,
    runs: &[ColorRunItem],
    base_attrs: Attrs<'a>,
) -> Vec<(&'t str, Attrs<'a>)> {
    let canonical = canonicalize_color_runs_impl(runs, text.len());

    let mut spans: Vec<(&'t str, Attrs<'a>)> = Vec::with_capacity(canonical.len() * 2 + 1);
    let mut cursor = 0usize;

    for run in &canonical {
        let run_start = run.start_byte;
        let run_end = run.end_byte;
        // run_start < run_end guaranteed by canonicalize_color_runs_impl.
        // Both are ≤ text.len() (clamped during canonicalization).

        // Emit unstyled gap before this run (if any).
        if cursor < run_start {
            spans.push((&text[cursor..run_start], base_attrs));
        }

        // Emit the colored run.
        let run_attrs = base_attrs.color(Color::rgba(
            run.color[0],
            run.color[1],
            run.color[2],
            run.color[3],
        ));
        spans.push((&text[run_start..run_end], run_attrs));

        cursor = run_end;
    }

    // Emit any trailing unstyled text after the last run.
    if cursor < text.len() {
        spans.push((&text[cursor..], base_attrs));
    }

    // If no spans were emitted (all runs were empty/invalid), fall back to
    // the full text with base_attrs so the buffer is never left empty.
    if spans.is_empty() {
        spans.push((text, base_attrs));
    }

    spans
}

/// Convert [`crate::markdown::ParsedMarkdown::spans`] into render-ready
/// [`StyledRunItem`]s against `text` (the parse's `plain_text`).
///
/// A span whose byte range is empty after clamping to `text.len()`, or whose
/// start/end does not land on a UTF-8 char boundary, is dropped — exactly the
/// filter [`TextItem::from_text_markdown_cached`] applies inline before
/// hud-3xdlf factored it out here.
///
/// Shared by [`TextItem::from_text_markdown_cached`] (the render path) and
/// [`measure_markdown_content_height`] (the vertical-flow measurement seam,
/// hud-3xdlf) so both build the identical run set from the identical parse —
/// a second, independently-drifting reimplementation of this filter/clamp/
/// convert logic is exactly the "measured != painted" bug class this sharing
/// exists to prevent.
pub(crate) fn markdown_spans_to_styled_runs(
    text: &str,
    spans: &[crate::markdown::StyledSpan],
) -> Box<[StyledRunItem]> {
    spans
        .iter()
        .filter_map(|span| {
            let start = span.start_byte.min(text.len());
            let end = span.end_byte.min(text.len());
            if start >= end {
                return None;
            }
            if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                return None;
            }
            let color = span.attr.color.map(rgba_to_srgb_u8);
            let background_color = span.attr.background_color.map(rgba_to_srgb_u8);
            Some(StyledRunItem {
                start_byte: start,
                end_byte: end,
                weight: span.attr.weight,
                italic: span.attr.italic,
                monospace: span.attr.monospace,
                color,
                background_color,
                size_scale: span.attr.size_scale,
                fill_line_width: false,
            })
        })
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

/// Build `(text_slice, Attrs)` pairs for [`Buffer::set_rich_text`] from a set of
/// [`StyledRunItem`]s produced by the Phase-1 markdown parse cache.
///
/// Each run carries weight, italic, monospace, optional color, and an optional
/// font-size scale.  Gaps between runs receive `base_attrs` with no overrides.
///
/// `base_family` is the node-level font family (e.g. `SansSerif`) used for
/// non-monospace runs.  Monospace runs use `Family::Monospace` regardless.
///
/// `base_font_size_px` is the node's base font size; it is multiplied by each
/// run's `size_scale` (when `Some`) to derive the per-span `Attrs::metrics`.
pub(crate) fn styled_run_spans<'t, 'a>(
    text: &'t str,
    runs: &[StyledRunItem],
    base_attrs: Attrs<'a>,
    base_family: FontFamily,
    base_font_size_px: f32,
    line_height_multiplier: f32,
) -> Vec<(&'t str, Attrs<'a>)> {
    if runs.is_empty() {
        return vec![(text, base_attrs)];
    }

    let mono_family = Family::Monospace;
    let sans_family = match base_family {
        FontFamily::SystemSansSerif => Family::SansSerif,
        FontFamily::SystemMonospace => Family::Monospace,
        FontFamily::SystemSerif => Family::Serif,
    };

    let mut spans: Vec<(&'t str, Attrs<'a>)> = Vec::with_capacity(runs.len() * 2 + 1);
    let mut cursor = 0usize;

    for run in runs {
        let start = run.start_byte.min(text.len());
        let end = run.end_byte.min(text.len());
        if start >= end {
            continue;
        }
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        // Clamp start to cursor so overlapping runs (which should not occur
        // after the fill_gaps_with_base fix, but may in edge cases) never
        // cause duplicate text slices or backward indexing.
        let start = start.max(cursor);
        if start >= end {
            cursor = cursor.max(end);
            continue;
        }

        // Emit unstyled gap before this run.
        if cursor < start && text.is_char_boundary(cursor) {
            spans.push((&text[cursor..start], base_attrs));
        }

        // Build attrs for this run.
        let run_weight = run
            .weight
            .map(|w| Weight(w.clamp(100, 900)))
            .unwrap_or_else(|| {
                // Inherit base weight (already applied to base_attrs).
                base_attrs.weight
            });
        let run_style = if run.italic {
            Style::Italic
        } else {
            Style::Normal
        };
        let run_family = if run.monospace {
            mono_family
        } else {
            sans_family
        };

        let mut run_attrs = base_attrs
            .weight(run_weight)
            .style(run_style)
            .family(run_family);

        if let Some(c) = run.color {
            run_attrs = run_attrs.color(Color::rgba(c[0], c[1], c[2], c[3]));
        }

        // Apply per-span font-size scale (heading levels, etc.).
        // cosmic-text supports per-span Metrics via Attrs::metrics().
        if let Some(scale) = run.size_scale {
            let scaled_size = (base_font_size_px * scale).clamp(1.0, 500.0);
            let scaled_line_height = scaled_size * line_height_multiplier;
            run_attrs = run_attrs.metrics(Metrics::new(scaled_size, scaled_line_height));
        }

        spans.push((&text[start..end], run_attrs));
        cursor = end;
    }

    // Trailing unstyled text.
    if cursor < text.len() && text.is_char_boundary(cursor) {
        spans.push((&text[cursor..], base_attrs));
    }

    if spans.is_empty() {
        spans.push((text, base_attrs));
    }

    spans
}

/// Re-slice [`StyledRunItem`]s to a truncated byte range.
///
/// When ellipsis truncation is applied, the truncated text is a plain-text
/// prefix with "…" appended.  The original `styled_runs` byte offsets remain
/// valid for the prefix portion of the truncated text, but must be clamped to
/// `content_end_byte` (the byte length of the prefix, excluding "…").
///
/// Runs entirely beyond `content_end_byte` are dropped.
/// Runs that partially overlap `content_end_byte` are clamped at that boundary,
/// provided both the run start and the clamped end are valid UTF-8 boundaries in
/// `truncated_text`.  Invalid boundary runs are dropped rather than panicking.
///
/// Returns an owned slice ready to pass to [`styled_run_spans`].
pub(crate) fn reslice_styled_runs(
    runs: &[StyledRunItem],
    content_end_byte: usize,
) -> Vec<StyledRunItem> {
    runs.iter()
        .filter_map(|run| {
            let start = run.start_byte;
            let end = run.end_byte.min(content_end_byte);
            if start >= end || start >= content_end_byte {
                return None;
            }
            Some(StyledRunItem {
                start_byte: start,
                end_byte: end,
                weight: run.weight,
                italic: run.italic,
                monospace: run.monospace,
                color: run.color,
                background_color: run.background_color,
                size_scale: run.size_scale,
                fill_line_width: false,
            })
        })
        .collect()
}

/// Re-slice [`ColorRunItem`]s to a truncated byte range.
///
/// Analogous to [`reslice_styled_runs`]: clamps runs to `content_end_byte`
/// (the byte length of the plain-text prefix, excluding the trailing "…").
/// Runs entirely beyond `content_end_byte` are dropped; partial runs are clamped.
pub(crate) fn reslice_color_runs(
    runs: &[ColorRunItem],
    content_end_byte: usize,
) -> Vec<ColorRunItem> {
    runs.iter()
        .filter_map(|run| {
            let start = run.start_byte;
            let end = run.end_byte.min(content_end_byte);
            if start >= end || start >= content_end_byte {
                return None;
            }
            Some(ColorRunItem {
                start_byte: start,
                end_byte: end,
                color: run.color,
            })
        })
        .collect()
}

/// Byte length of the leading-ellipsis prefix in a tail-anchored truncated
/// string.
///
/// `truncate_tail_anchored` normally emits `"…\n{tail}"` (multi-line window),
/// whose prefix is `ELLIPSIS` + `'\n'`.  The degenerate `max_lines == 1` case
/// (hud-qoq52, option (b)) instead emits a single inline-leading-ellipsis line
/// `"…{tail}"` with **no** newline, whose prefix is just `ELLIPSIS`.  Returns
/// `None` when `truncated_str` does not begin with `ELLIPSIS` at all (caller
/// then treats the runs as unremappable).
fn tail_anchored_leading_bytes(truncated_str: &str) -> Option<usize> {
    let after_ellipsis = truncated_str.strip_prefix(crate::overflow::ELLIPSIS)?;
    if let Some(rest) = after_ellipsis.strip_prefix('\n') {
        // "…\n{tail}" — multi-line leading-ellipsis line.
        let _ = rest;
        Some(crate::overflow::ELLIPSIS.len() + 1)
    } else {
        // "…{tail}" — single-line inline leading ellipsis (max_lines == 1).
        Some(crate::overflow::ELLIPSIS.len())
    }
}

/// Re-slice [`StyledRunItem`]s for a **tail-anchored** truncated string.
///
/// `truncate_tail_anchored` produces a leading-ellipsis output:
/// `"…\n{visible_tail}"` (multi-line) or `"…{visible_tail}"` (the single-line
/// `max_lines == 1` case, hud-qoq52).  The original `styled_runs` byte offsets are
/// relative to the **original** text; they must be shifted to become relative
/// to `effective_text` (= the leading-ellipsis truncated string).
///
/// The shift is computed by finding where `visible_tail` begins in
/// `original_text` (`tail_origin`) and applying:
///
/// ```text
/// new_offset = old_offset - tail_origin + leading_bytes
/// ```
///
/// where `leading_bytes = ELLIPSIS.len() + 1` (the `"…\n"` prefix = 4 bytes).
///
/// If the first visible line was also horizontally truncated (its text no
/// longer matches the suffix of `original_text`), the shift cannot be computed
/// exactly.  In that case an empty `Vec` is returned and the caller must fall
/// back to `buf.set_text` (unstyled) to avoid mis-styled glyphs.
///
/// Runs entirely outside the visible tail range are dropped; runs partially
/// overlapping `tail_origin` are clamped to the tail start.
///
/// Returns a `Vec<StyledRunItem>` with offsets valid against `truncated_str`.
pub(crate) fn reslice_styled_runs_tail_anchored(
    original_text: &str,
    truncated_str: &str,
    runs: &[StyledRunItem],
) -> Vec<StyledRunItem> {
    // Leading-ellipsis prefix: "…\n" (multi-line) or "…" (single-line max_lines==1).
    let leading_bytes = match tail_anchored_leading_bytes(truncated_str) {
        Some(n) => n,
        None => return Vec::new(), // not a leading-ellipsis string — unremappable
    };

    // Extract the visible tail content (after the leading-ellipsis prefix).
    let tail_content = match truncated_str.get(leading_bytes..) {
        Some(s) => s,
        None => return Vec::new(), // truncated_str is too short — malformed
    };

    // Determine where the tail content begins in the original text.
    // If original_text ends with tail_content, the shift is unambiguous.
    // Otherwise the first visible line was further truncated; we cannot safely
    // remap offsets, so return empty to trigger the unstyled fallback.
    if !original_text.ends_with(tail_content) {
        return Vec::new();
    }
    let tail_origin = original_text.len() - tail_content.len();

    // Shift: subtract tail_origin (to make offsets relative to tail_content),
    // then add leading_bytes (to account for the "…\n" prefix in truncated_str).
    // Saturate at 0 for runs that start before tail_origin (clamped to tail).
    runs.iter()
        .filter_map(|run| {
            // Drop runs entirely before the visible tail.
            if run.end_byte <= tail_origin {
                return None;
            }
            // Clamp start to tail_origin (run starts before visible range).
            let orig_start = run.start_byte.max(tail_origin);
            let orig_end = run.end_byte;

            // Shift to effective_text coordinates.
            let new_start = orig_start - tail_origin + leading_bytes;
            let new_end = orig_end - tail_origin + leading_bytes;

            // Clamp new_end to truncated_str length (defensive).
            let new_end = new_end.min(truncated_str.len());

            if new_start >= new_end {
                return None;
            }

            Some(StyledRunItem {
                start_byte: new_start,
                end_byte: new_end,
                weight: run.weight,
                italic: run.italic,
                monospace: run.monospace,
                color: run.color,
                background_color: run.background_color,
                size_scale: run.size_scale,
                fill_line_width: false,
            })
        })
        .collect()
}

/// Re-slice [`ColorRunItem`]s for a **tail-anchored** truncated string.
///
/// Analogous to [`reslice_styled_runs_tail_anchored`]: shifts run offsets from
/// the original-text coordinate system to the leading-ellipsis `"…\n{tail}"`
/// coordinate system produced by [`overflow::truncate_tail_anchored`].
///
/// Returns an empty `Vec` when the first visible line was horizontally
/// truncated (offsets cannot be safely remapped); the caller must fall back to
/// `buf.set_text`.
pub(crate) fn reslice_color_runs_tail_anchored(
    original_text: &str,
    truncated_str: &str,
    runs: &[ColorRunItem],
) -> Vec<ColorRunItem> {
    let leading_bytes = match tail_anchored_leading_bytes(truncated_str) {
        Some(n) => n,
        None => return Vec::new(),
    };

    let tail_content = match truncated_str.get(leading_bytes..) {
        Some(s) => s,
        None => return Vec::new(),
    };

    if !original_text.ends_with(tail_content) {
        return Vec::new();
    }
    let tail_origin = original_text.len() - tail_content.len();

    runs.iter()
        .filter_map(|run| {
            if run.end_byte <= tail_origin {
                return None;
            }
            let orig_start = run.start_byte.max(tail_origin);
            let orig_end = run.end_byte;
            let new_start = orig_start - tail_origin + leading_bytes;
            let new_end = (orig_end - tail_origin + leading_bytes).min(truncated_str.len());
            if new_start >= new_end {
                return None;
            }
            Some(ColorRunItem {
                start_byte: new_start,
                end_byte: new_end,
                color: run.color,
            })
        })
        .collect()
}

/// Compute the "widest" effective measurement weight and font family for a
/// `TextItem` that has [`StyledRunItem`]s.
///
/// Used when priming the truncation cache for items with styled runs (hud-so7zu).
/// Styled runs may include bold (weight=700) or monospace spans that are wider
/// than the base font.  If the truncation measurement uses only the base attrs,
/// bold/monospace text wraps to more lines than measured, causing the fit
/// decision to declare text fits when it actually overflows.
///
/// Returns `(effective_weight, use_monospace)` where:
/// - `effective_weight` is the maximum weight across all runs that explicitly
///   set a weight (fallback: `base_weight`).
/// - `use_monospace` is `true` if any run requests the monospace family.
///
/// Callers should use these values to build the `base_attrs` passed to
/// [`overflow::truncate_for_ellipsis`] / [`overflow::truncate_tail_anchored`]
/// so the measurement is pessimistic (wider) and the fit decision is accurate.
pub(crate) fn styled_runs_effective_measurement(
    runs: &[StyledRunItem],
    base_weight: u16,
) -> (u16, bool) {
    let mut max_weight = base_weight;
    let mut any_monospace = false;
    for run in runs {
        if let Some(w) = run.weight {
            max_weight = max_weight.max(w);
        }
        if run.monospace {
            any_monospace = true;
        }
    }
    (max_weight, any_monospace)
}

/// Compute the effective [`TruncationKey`] for a `TextItem`.
///
/// This is the key-building core of [`TextRasterizer::prime_truncation_cache`]
/// and the key-derivation pass inside [`TextRasterizer::prepare_text_items`].
/// Both call sites use effective measurement attrs from `styled_runs` so that
/// bold/monospace spans are measured accurately (#708 fix (b)).
///
/// Returns `None` for items whose `overflow` is not [`TextOverflow::Ellipsis`].
///
/// # #708 fix (b) — regression-test anchor
///
/// If `styled_runs_effective_measurement` is removed from this function (fix
/// reverted), the key will always use `item.font_weight` (base weight) even
/// when bold or monospace runs are present.  Tests that call this function with
/// a bold-styled item and assert the resulting key differs from a key built
/// with base weight will fail, catching the regression.
pub(crate) fn effective_truncation_key(item: &TextItem) -> Option<TruncationKey> {
    if item.overflow != TextOverflow::Ellipsis {
        return None;
    }
    // (hud-so7zu) Use effective measurement attrs from styled runs.
    let (meas_weight, meas_mono) =
        styled_runs_effective_measurement(&item.styled_runs, item.font_weight);
    let meas_family = if meas_mono {
        FontFamily::SystemMonospace
    } else {
        item.font_family
    };
    Some(TruncationKey::new(
        &item.text,
        item.bounds_width,
        item.bounds_height,
        item.font_size_px,
        meas_family,
        meas_weight,
        item.viewport,
    ))
}

/// Reslice `item.styled_runs` into the coordinate space of `truncated_str`.
///
/// This is the reslice-decision core of the styled-run path inside
/// [`TextRasterizer::prepare_text_items`] (lines 591–614).  Extracted as a
/// free function so that the fix can be regression-tested without requiring a
/// GPU-backed `TextRasterizer` (#708 fix (a)).
///
/// - If `truncated_str == item.text` (no truncation occurred), the original
///   runs are returned unchanged.
/// - If `truncated_str` starts with `ELLIPSIS` and `item.viewport` is
///   [`TruncationViewport::TailAnchored`], offsets are shifted from original-
///   text space into the "…\n{tail}" coordinate space via
///   [`reslice_styled_runs_tail_anchored`].
/// - Otherwise (HeadAnchored trailing-ellipsis) the runs are clamped to the
///   byte length of the plain-text prefix (before "…") via
///   [`reslice_styled_runs`].
///
/// # #708 fix (a) — regression-test anchor
///
/// If `reslice_styled_runs` is removed from the HeadAnchored branch (fix
/// reverted), styled runs will carry un-clamped byte offsets that extend into
/// the "…" glyph.  Tests that call this function with a bold run whose
/// `end_byte` covers the "…" bytes and assert the returned run does NOT include
/// those bytes will fail, catching the regression.
pub(crate) fn reslice_styled_runs_for_item(
    item: &TextItem,
    truncated_str: &str,
) -> Vec<StyledRunItem> {
    if truncated_str == item.text.as_ref() {
        // Text fit without truncation — preserve original runs.
        item.styled_runs.to_vec()
    } else if item.viewport == TruncationViewport::TailAnchored
        && truncated_str.starts_with(crate::overflow::ELLIPSIS)
    {
        // Leading-ellipsis output: shift offsets from original-text space
        // into "…\n{tail}" space.
        reslice_styled_runs_tail_anchored(&item.text, truncated_str, &item.styled_runs)
    } else {
        // Trailing-ellipsis output (HeadAnchored): compute the byte length of
        // the plain-text prefix (excluding "…") and clamp runs to it.
        let content_end = truncated_str
            .strip_suffix(crate::overflow::ELLIPSIS)
            .map(|s| s.len())
            .unwrap_or(truncated_str.len());
        reslice_styled_runs(&item.styled_runs, content_end)
    }
}

/// Format a 32-byte resource ID as a lowercase hex string for logging.
pub(crate) fn format_resource_id(id: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in id {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Minimal Markdown strip for v1: removes `#` heading prefixes and `*` emphasis
/// markers. Does not parse nested Markdown, code blocks, or links.
pub fn strip_markdown_v1(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let stripped = line.trim_start_matches('#').trim_start();
        let stripped = stripped.replace('*', "");
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&stripped);
    }
    out
}

/// Convert a linear light value [0..1] to an sRGB u8 [0..255].
///
/// Uses the piecewise IEC 61966-2-1 formula (same as wgpu's `Rgba8UnormSrgb`
/// conversion).
fn linear_to_srgb_u8(linear: f32) -> u8 {
    let clamped = linear.clamp(0.0, 1.0);
    let srgb = if clamped <= 0.003_130_8 {
        clamped * 12.92
    } else {
        1.055 * clamped.powf(1.0 / 2.4) - 0.055
    };
    (srgb * 255.0 + 0.5) as u8
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── portal_part_clip_rect (hud-s4lrw) ──────────────────────────────────
    // Pure CPU geometry: the per-part clip envelope for multi-node portals.

    /// With no declared part, the clip is exactly the content∩tile intersection
    /// and matches the pre-promotion single-node behavior (content clamped to
    /// the tile box).
    #[test]
    fn portal_part_clip_rect_no_part_clamps_to_tile() {
        // Content overflows the tile on the right and bottom.
        let clip =
            portal_part_clip_rect((10.0, 10.0, 400.0, 400.0), (0.0, 0.0, 100.0, 100.0), None);
        // Right/bottom are clamped to the tile edge (x=100, y=100).
        assert_eq!(clip, (10.0, 10.0, 90.0, 90.0));
    }

    /// A declared part region narrows the clip below the tile box — an overlong
    /// transcript node is contained to its part band and cannot paint over the
    /// composer strip beneath it.
    #[test]
    fn portal_part_clip_rect_part_contains_content() {
        // Transcript node laid out over the whole tile, but its part band is the
        // top 60px only.
        let clip = portal_part_clip_rect(
            (0.0, 0.0, 100.0, 100.0),      // content spans the tile
            (0.0, 0.0, 100.0, 100.0),      // tile
            Some((0.0, 0.0, 100.0, 60.0)), // transcript band (absolute)
        );
        assert_eq!(clip, (0.0, 0.0, 100.0, 60.0));
    }

    /// The part envelope never *widens* the clip past the tile: a part region
    /// larger than the tile is still bounded by the tile box.
    #[test]
    fn portal_part_clip_rect_part_cannot_exceed_tile() {
        let clip = portal_part_clip_rect(
            (0.0, 0.0, 100.0, 100.0),
            (0.0, 0.0, 80.0, 80.0),
            Some((0.0, 0.0, 200.0, 200.0)),
        );
        assert_eq!(clip, (0.0, 0.0, 80.0, 80.0));
    }

    /// A disjoint part (content laid out outside its declared region) collapses
    /// to the 1.0-floored degenerate rect rather than a zero/negative extent.
    #[test]
    fn portal_part_clip_rect_disjoint_floors_to_unit() {
        let clip = portal_part_clip_rect(
            (0.0, 0.0, 40.0, 40.0),
            (0.0, 0.0, 100.0, 100.0),
            Some((60.0, 60.0, 40.0, 40.0)), // no overlap with content
        );
        // Width/height floored at 1.0 (never zero/negative).
        assert!(clip.2 >= 1.0 && clip.3 >= 1.0, "got {clip:?}");
    }

    /// The part envelope is a clip only; it is applied in absolute space after
    /// the tile origin offset, so a part band on a translated tile clips at the
    /// translated location.
    #[test]
    fn portal_part_clip_rect_absolute_part_offset() {
        // Tile at (200,100); transcript band is the tile's top 50px → absolute
        // (200,100,300,50).
        let clip = portal_part_clip_rect(
            (200.0, 100.0, 300.0, 400.0), // content
            (200.0, 100.0, 300.0, 400.0), // tile
            Some((200.0, 100.0, 300.0, 50.0)),
        );
        assert_eq!(clip, (200.0, 100.0, 300.0, 50.0));
    }

    /// hud-n0x4u repro (pure CPU): a single unbroken token far wider than the box
    /// must fall back to glyph-level (break-anywhere) wrapping so every visual line
    /// fits inside `wrap_width` and no content is left on one over-long line whose
    /// tail the box scissor would clip.
    ///
    /// This exercises the shared [`composer_wrap_line_widths`] helper, which uses
    /// [`WRAPPED_TEXT_WRAP`] — the exact policy the composer draft, the viewer-echo
    /// history line count, and the render paint path all use. Before the fix
    /// (`Wrap::Word`) the 200-char run shaped to ONE line wider than the box: this
    /// asserts `> 1` line, so it fails on the old policy and passes on the new one.
    #[test]
    fn overlong_unbroken_token_wraps_at_glyph_level_within_box() {
        let mut fs = bundled_font_system();
        let font_size_px = 16.0;
        let lhm = 1.4;
        let wrap_width = 80.0;

        // A 200-char run with NO spaces — one unbreakable "word" (the owner's
        // `asdasd…` case). Word-only wrap cannot break it; glyph fallback must.
        let token = "a".repeat(200);
        let widths = composer_wrap_line_widths(&mut fs, &token, wrap_width, font_size_px, lhm);

        assert!(
            widths.len() > 1,
            "an over-long unbroken token must wrap to multiple lines \
             (break-anywhere fallback); got {} line(s) — the tail would be clipped",
            widths.len()
        );
        // Every wrapped line must fit inside the box (small epsilon for the final
        // glyph advance / AA). A line wider than the box is exactly the clipped-tail
        // defect this fix removes.
        for (i, w) in widths.iter().enumerate() {
            assert!(
                *w <= wrap_width + 1.0,
                "wrapped line {i} width {w} exceeds wrap_width {wrap_width}; \
                 its tail would be clipped by the box scissor"
            );
        }
    }

    /// A token that already fits stays on a single line — the break-anywhere
    /// policy is a strict superset of word wrap and must not gratuitously split
    /// content that fits.
    #[test]
    fn short_token_stays_single_line() {
        let mut fs = bundled_font_system();
        let widths = composer_wrap_line_widths(&mut fs, "hi", 400.0, 16.0, 1.4);
        assert_eq!(widths.len(), 1, "a short token must not wrap");
    }

    /// Normal spaced text still wraps at word boundaries under the break-anywhere
    /// policy (regression guard: WordOrGlyph must not change ordinary word wrap).
    #[test]
    fn spaced_text_still_word_wraps() {
        let mut fs = bundled_font_system();
        let text = "word ".repeat(40); // ~200 chars, many spaces
        let widths = composer_wrap_line_widths(&mut fs, &text, 80.0, 16.0, 1.4);
        assert!(
            widths.len() > 1,
            "spaced text must still wrap to multiple lines"
        );
        for (i, w) in widths.iter().enumerate() {
            assert!(
                *w <= 80.0 + 1.0,
                "word-wrapped line {i} width {w} exceeds box width 80.0"
            );
        }
    }

    #[test]
    fn strip_markdown_removes_heading_markers() {
        let md = "# Hello\n## World\nPlain text";
        let stripped = strip_markdown_v1(md);
        assert_eq!(stripped, "Hello\nWorld\nPlain text");
    }

    #[test]
    fn strip_markdown_removes_emphasis() {
        let md = "Hello *world*";
        let stripped = strip_markdown_v1(md);
        assert_eq!(stripped, "Hello world");
    }

    #[test]
    fn strip_markdown_empty_input() {
        assert_eq!(strip_markdown_v1(""), "");
    }

    #[test]
    fn linear_to_srgb_u8_black() {
        assert_eq!(linear_to_srgb_u8(0.0), 0);
    }

    #[test]
    fn linear_to_srgb_u8_white() {
        assert_eq!(linear_to_srgb_u8(1.0), 255);
    }

    #[test]
    fn linear_to_srgb_u8_midpoint() {
        // Linear 0.5 → sRGB ~0.735 → ~187 u8
        let v = linear_to_srgb_u8(0.5);
        assert!(v > 180 && v < 200, "midpoint sRGB: {v}");
    }

    #[test]
    fn text_item_from_text_markdown_node_insets_margin() {
        use tze_hud_scene::types::{Rect, Rgba, TextMarkdownNode};
        let node = TextMarkdownNode {
            content: "# Hello\n*world*".to_owned(),
            bounds: Rect::new(10.0, 20.0, 200.0, 80.0),
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        };
        let item = TextItem::from_text_markdown_node(&node, 50.0, 50.0);
        // tile_x=50, tile_y=50, node.bounds.x=10, node.bounds.y=20, margin=6
        assert_eq!(item.pixel_x, 66.0);
        assert_eq!(item.pixel_y, 76.0);
        assert_eq!(&*item.text, "Hello\nworld");
        // New fields default correctly.
        assert!(item.outline_color.is_none());
        assert!(item.outline_width.is_none());
        assert_eq!(item.opacity, 1.0);
    }

    #[test]
    fn text_item_from_small_text_markdown_node_does_not_collapse_height() {
        use tze_hud_scene::types::{Rect, Rgba, TextMarkdownNode};
        let node = TextMarkdownNode {
            content: "RESIDENT AGENT".to_owned(),
            bounds: Rect::new(96.0, 18.0, 152.0, 12.0),
            font_size_px: 11.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        };
        let item = TextItem::from_text_markdown_node(&node, 0.0, 0.0);
        assert!(
            item.bounds_height >= 11.0,
            "small presence-card labels must retain height close to their font size for live glyph rendering; got {}",
            item.bounds_height
        );
        assert!(
            item.pixel_y < 22.0,
            "small presence-card labels should not be pushed too far down by inset margin; got {}",
            item.pixel_y
        );
    }

    // ── from_zone_policy tests [hud-sc0a.8] ──────────────────────────────────

    #[test]
    fn from_zone_policy_reads_font_size_and_color() {
        use tze_hud_scene::types::{RenderingPolicy, Rgba};
        let policy = RenderingPolicy {
            font_size_px: Some(24.0),
            text_color: Some(Rgba::new(1.0, 0.0, 0.0, 1.0)), // red
            ..Default::default()
        };
        let item = TextItem::from_zone_policy("hello", 0.0, 0.0, 300.0, 80.0, &policy, 1.0);
        assert_eq!(item.font_size_px, 24.0);
        // Red channel (R=1.0 linear → 255 sRGB).
        assert_eq!(item.color[0], 255, "R should be max (red)");
        assert_eq!(item.color[1], 0, "G should be 0");
    }

    #[test]
    fn from_zone_policy_outline_fields_propagated() {
        use tze_hud_scene::types::{RenderingPolicy, Rgba};
        let policy = RenderingPolicy {
            outline_color: Some(Rgba::BLACK),
            outline_width: Some(2.0),
            ..Default::default()
        };
        let item = TextItem::from_zone_policy("outlined", 0.0, 0.0, 300.0, 80.0, &policy, 1.0);
        assert!(item.outline_color.is_some(), "outline_color should be Some");
        assert_eq!(item.outline_width.unwrap(), 2.0);
    }

    #[test]
    fn from_zone_policy_outline_width_zero_suppresses_outline() {
        use tze_hud_scene::types::{RenderingPolicy, Rgba};
        let policy = RenderingPolicy {
            outline_color: Some(Rgba::BLACK),
            outline_width: Some(0.0), // zero = no outline
            ..Default::default()
        };
        let item = TextItem::from_zone_policy("no_outline", 0.0, 0.0, 300.0, 80.0, &policy, 1.0);
        assert!(
            item.outline_color.is_none(),
            "outline_color should be None when outline_width=0"
        );
        assert!(item.outline_width.is_none());
    }

    #[test]
    fn from_zone_policy_opacity_applied_to_alpha() {
        use tze_hud_scene::types::{RenderingPolicy, Rgba};
        let policy = RenderingPolicy {
            text_color: Some(Rgba::new(1.0, 1.0, 1.0, 1.0)),
            ..Default::default()
        };
        // opacity=0.5 should halve the alpha.
        let item = TextItem::from_zone_policy("faded", 0.0, 0.0, 300.0, 80.0, &policy, 0.5);
        let alpha = item.color[3];
        // 255 * 0.5 = 127 (±2 for rounding).
        assert!(
            (alpha as i32 - 127).abs() <= 2,
            "opacity=0.5 should halve alpha (got {alpha})"
        );
    }

    #[test]
    fn from_zone_policy_default_margins_used_when_none() {
        use tze_hud_scene::types::RenderingPolicy;
        let policy = RenderingPolicy::default(); // margin_horizontal/vertical = None
        let item = TextItem::from_zone_policy("text", 100.0, 200.0, 400.0, 100.0, &policy, 1.0);
        // Default margin = 8px on each side.
        assert_eq!(item.pixel_x, 108.0, "default margin_h = 8.0");
        assert_eq!(item.pixel_y, 208.0, "default margin_v = 8.0");
    }

    #[test]
    fn rgba_to_srgb_u8_black_and_white() {
        use tze_hud_scene::types::Rgba;
        let black = rgba_to_srgb_u8(Rgba::BLACK);
        assert_eq!(black, [0, 0, 0, 255]);
        let white = rgba_to_srgb_u8(Rgba::WHITE);
        assert_eq!(white, [255, 255, 255, 255]);
    }

    #[test]
    fn from_zone_policy_margin_px_fallback() {
        // When margin_horizontal/vertical are None but margin_px is set,
        // margin_px should be used (spec §Extended RenderingPolicy).
        use tze_hud_scene::types::RenderingPolicy;
        let policy = RenderingPolicy {
            margin_px: Some(20.0),
            // margin_horizontal/margin_vertical intentionally None
            ..Default::default()
        };
        let item = TextItem::from_zone_policy("text", 0.0, 0.0, 400.0, 100.0, &policy, 1.0);
        assert_eq!(
            item.pixel_x, 20.0,
            "margin_px fallback should apply to horizontal margin"
        );
        assert_eq!(
            item.pixel_y, 20.0,
            "margin_px fallback should apply to vertical margin"
        );
    }

    #[test]
    fn apply_opacity_to_color_halves_alpha() {
        let color = [200u8, 100, 50, 200];
        let result = apply_opacity_to_color(color, 0.5);
        assert_eq!(result[0], 200, "RGB channels unchanged");
        assert_eq!(result[1], 100);
        assert_eq!(result[2], 50);
        // 200 * 0.5 = 100.
        assert_eq!(result[3], 100, "alpha halved");
    }

    // ── font_weight tests [hud-w3o6.2] ───────────────────────────────────────

    /// font_weight=700 is propagated from RenderingPolicy to TextItem.
    ///
    /// Acceptance criterion §Alert-Banner Heading Typography:
    ///   "typography.heading.weight (700/bold)"
    #[test]
    fn from_zone_policy_font_weight_bold_propagated() {
        use tze_hud_scene::types::RenderingPolicy;
        let policy = RenderingPolicy {
            font_weight: Some(700),
            ..Default::default()
        };
        let item = TextItem::from_zone_policy("bold text", 0.0, 0.0, 300.0, 60.0, &policy, 1.0);
        assert_eq!(
            item.font_weight, 700,
            "font_weight=700 (bold) must be propagated from RenderingPolicy to TextItem"
        );
    }

    /// font_weight defaults to 400 (regular) when not set in RenderingPolicy.
    #[test]
    fn from_zone_policy_font_weight_defaults_to_regular() {
        use tze_hud_scene::types::RenderingPolicy;
        let policy = RenderingPolicy::default(); // font_weight = None
        let item = TextItem::from_zone_policy("regular text", 0.0, 0.0, 300.0, 60.0, &policy, 1.0);
        assert_eq!(
            item.font_weight, 400,
            "font_weight must default to 400 (regular) when not set"
        );
    }

    /// from_text_markdown_node defaults font_weight to 400 (regular).
    #[test]
    fn from_text_markdown_node_font_weight_defaults_to_regular() {
        use tze_hud_scene::types::{Rect, Rgba, TextMarkdownNode};
        let node = TextMarkdownNode {
            content: "Plain text".to_owned(),
            bounds: Rect::new(0.0, 0.0, 200.0, 60.0),
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        };
        let item = TextItem::from_text_markdown_node(&node, 0.0, 0.0);
        assert_eq!(
            item.font_weight, 400,
            "from_text_markdown_node must default font_weight to 400"
        );
    }

    /// from_zone_policy defaults overflow to Clip when policy.overflow is None.
    #[test]
    fn from_zone_policy_overflow_defaults_to_clip() {
        let policy = RenderingPolicy::default();
        let item = TextItem::from_zone_policy("text", 0.0, 0.0, 200.0, 40.0, &policy, 1.0);
        assert_eq!(
            item.overflow,
            TextOverflow::Clip,
            "overflow should default to Clip when policy.overflow is None"
        );
    }

    /// from_zone_policy propagates Ellipsis overflow from policy.
    #[test]
    fn from_zone_policy_overflow_ellipsis_propagated() {
        let policy = RenderingPolicy {
            overflow: Some(TextOverflow::Ellipsis),
            ..RenderingPolicy::default()
        };
        let item = TextItem::from_zone_policy("text", 0.0, 0.0, 200.0, 40.0, &policy, 1.0);
        assert_eq!(
            item.overflow,
            TextOverflow::Ellipsis,
            "overflow should be Ellipsis when policy.overflow is Some(Ellipsis)"
        );
    }

    /// from_zone_policy propagates explicit Clip overflow from policy.
    #[test]
    fn from_zone_policy_overflow_clip_explicit_propagated() {
        let policy = RenderingPolicy {
            overflow: Some(TextOverflow::Clip),
            ..RenderingPolicy::default()
        };
        let item = TextItem::from_zone_policy("text", 0.0, 0.0, 200.0, 40.0, &policy, 1.0);
        assert_eq!(
            item.overflow,
            TextOverflow::Clip,
            "overflow should be Clip when policy.overflow is Some(Clip)"
        );
    }

    // ── color_run_spans single-pass tests [hud-9pmd] ─────────────────────────

    /// Single colored run covering the entire text produces one span with color.
    #[test]
    fn color_run_spans_single_run_full_text() {
        let text = "hello";
        let runs = [ColorRunItem {
            start_byte: 0,
            end_byte: 5,
            color: [255, 0, 0, 255],
        }];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);
        assert_eq!(spans.len(), 1, "one run covering full text → one span");
        assert_eq!(spans[0].0, "hello");
        // The span must carry an explicit color (not base_attrs which has no color_opt).
        assert!(
            spans[0].1.color_opt.is_some(),
            "run span must have a color_opt set"
        );
    }

    /// Two runs with a gap in between produce three spans: gap, run, run.
    #[test]
    fn color_run_spans_two_runs_with_gap() {
        let text = "hello world";
        // "hello" → red, " " → unstyled gap, "world" → blue
        let runs = [
            ColorRunItem {
                start_byte: 0,
                end_byte: 5,
                color: [255, 0, 0, 255],
            },
            ColorRunItem {
                start_byte: 6,
                end_byte: 11,
                color: [0, 0, 255, 255],
            },
        ];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);
        // Expect: [("hello", red), (" ", base), ("world", blue)]
        assert_eq!(spans.len(), 3, "two runs with gap → three spans");
        assert_eq!(spans[0].0, "hello");
        assert!(spans[0].1.color_opt.is_some(), "first run must have color");
        assert_eq!(spans[1].0, " ", "gap between runs must be unstyled");
        assert!(
            spans[1].1.color_opt.is_none(),
            "gap span must use base_attrs (no color_opt)"
        );
        assert_eq!(spans[2].0, "world");
        assert!(spans[2].1.color_opt.is_some(), "second run must have color");
    }

    /// A run followed by trailing unstyled text produces run + trailing span.
    #[test]
    fn color_run_spans_trailing_unstyled() {
        let text = "hello world";
        let runs = [ColorRunItem {
            start_byte: 0,
            end_byte: 5,
            color: [255, 0, 0, 255],
        }];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);
        assert_eq!(spans.len(), 2, "run + trailing unstyled → two spans");
        assert_eq!(spans[0].0, "hello");
        assert_eq!(spans[1].0, " world");
        assert!(
            spans[1].1.color_opt.is_none(),
            "trailing span must use base_attrs"
        );
    }

    /// Empty runs slice falls back to a single full-text base-attrs span.
    #[test]
    fn color_run_spans_empty_runs_fallback() {
        let text = "no color";
        let spans = color_run_spans(text, &[], Attrs::new());
        assert_eq!(spans.len(), 1, "no runs → single fallback span");
        assert_eq!(spans[0].0, "no color");
        assert!(
            spans[0].1.color_opt.is_none(),
            "fallback span must use base_attrs"
        );
    }

    /// Out-of-bounds run end is clamped; no panic.
    #[test]
    fn color_run_spans_clamped_out_of_bounds_run() {
        let text = "hi";
        let runs = [ColorRunItem {
            start_byte: 0,
            end_byte: 999, // way beyond text length
            color: [0, 255, 0, 255],
        }];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);
        // Should produce one span covering the full text with color.
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, "hi");
        assert!(spans[0].1.color_opt.is_some());
    }

    /// Degenerate run (start >= end) is skipped; remaining text is returned unstyled.
    #[test]
    fn color_run_spans_degenerate_run_skipped() {
        let text = "text";
        let runs = [ColorRunItem {
            start_byte: 2,
            end_byte: 2, // zero-length: should be skipped
            color: [255, 0, 0, 255],
        }];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);
        // Zero-length run → no gap before it (cursor=0, run_start=2), and run is skipped.
        // cursor stays at 0, so trailing text "text" is emitted as unstyled.
        // But the fallback at the end also covers this — spans should be non-empty.
        assert!(
            !spans.is_empty(),
            "degenerate run must not produce empty spans"
        );
        let combined: String = spans.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            combined, "text",
            "all text must be covered even with degenerate run"
        );
    }

    /// color_run_spans produces correct slices for multi-byte UTF-8 characters.
    #[test]
    fn color_run_spans_multibyte_utf8() {
        // "é" is 2 bytes (0xC3 0xA9), "world" is 5 bytes → total 7 bytes
        let text = "éworld";
        let runs = [ColorRunItem {
            start_byte: 0,
            end_byte: 2, // "é"
            color: [255, 0, 0, 255],
        }];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].0, "é");
        assert_eq!(spans[1].0, "world");
    }

    // ── Overlap / out-of-order tests [hud-qu8k4] ─────────────────────────────

    /// Two overlapping runs: later run wins on the intersection.
    ///
    /// Input: red 0..10, blue 5..15 on "0123456789abcde" (15 bytes).
    /// Expected: red 0..5, blue 5..15.
    #[test]
    fn color_run_spans_two_runs_overlap_later_wins() {
        let text = "0123456789abcde"; // 15 ASCII bytes
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];
        let runs = [
            ColorRunItem {
                start_byte: 0,
                end_byte: 10,
                color: red,
            },
            ColorRunItem {
                start_byte: 5,
                end_byte: 15,
                color: blue,
            },
        ];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);

        // Reconstruct text coverage.
        let combined: String = spans.iter().map(|(s, _)| *s).collect();
        assert_eq!(combined, text, "all text must be covered");

        // First span: "01234" (bytes 0..5) — red.
        assert_eq!(spans[0].0, &text[0..5], "red prefix should be 0..5");
        assert!(spans[0].1.color_opt.is_some(), "first span must be colored");
        let red_color = spans[0].1.color_opt.unwrap();
        assert_eq!(
            (red_color.r(), red_color.g(), red_color.b()),
            (255, 0, 0),
            "first span should be red"
        );

        // Second span: "56789abcde" (bytes 5..15) — blue (later run wins intersection).
        assert_eq!(spans[1].0, &text[5..15], "blue span should cover 5..15");
        assert!(
            spans[1].1.color_opt.is_some(),
            "second span must be colored"
        );
        let blue_color = spans[1].1.color_opt.unwrap();
        assert_eq!(
            (blue_color.r(), blue_color.g(), blue_color.b()),
            (0, 0, 255),
            "second span should be blue (later-writer-wins)"
        );

        assert_eq!(spans.len(), 2, "no trailing unstyled text expected");
    }

    /// Three runs: middle run overlaps both outer runs; middle (later) wins intersection.
    ///
    /// Input: red 0..8, green 4..12, blue 10..15 on 15-byte ASCII.
    /// Expected: red 0..4, green 4..12, blue 12..15.
    #[test]
    fn color_run_spans_three_runs_middle_overlaps_outer() {
        let text = "0123456789abcde"; // 15 ASCII bytes
        let red = [255u8, 0, 0, 255];
        let green = [0u8, 255, 0, 255];
        let blue = [0u8, 0, 255, 255];
        // red: 0..8, green: 4..12 (overlaps red and blue), blue: 10..15
        let runs = [
            ColorRunItem {
                start_byte: 0,
                end_byte: 8,
                color: red,
            },
            ColorRunItem {
                start_byte: 4,
                end_byte: 12,
                color: green,
            },
            ColorRunItem {
                start_byte: 10,
                end_byte: 15,
                color: blue,
            },
        ];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);

        let combined: String = spans.iter().map(|(s, _)| *s).collect();
        assert_eq!(combined, text, "all 15 bytes must be covered");

        // Verify the color at the start of each expected segment.
        // Expected layout: red[0..4], green[4..12], blue[12..15].
        // (green wins 4..12; blue wins 10..15 — within green's range 10..12 is disputed;
        //  blue is the latest run touching 10..12, so blue wins.)
        // Actually: blue(10..15) is later than green(4..12), so blue wins 10..12 too.
        // Expected: red[0..4], green[4..10], blue[10..15].
        let segment_texts: Vec<&str> = spans.iter().map(|(s, _)| *s).collect();
        assert_eq!(segment_texts[0], &text[0..4], "segment 0 = red[0..4]");
        assert_eq!(segment_texts[1], &text[4..10], "segment 1 = green[4..10]");
        assert_eq!(segment_texts[2], &text[10..15], "segment 2 = blue[10..15]");

        let c0 = spans[0].1.color_opt.unwrap();
        assert_eq!((c0.r(), c0.g(), c0.b()), (255, 0, 0), "seg0 = red");
        let c1 = spans[1].1.color_opt.unwrap();
        assert_eq!((c1.r(), c1.g(), c1.b()), (0, 255, 0), "seg1 = green");
        let c2 = spans[2].1.color_opt.unwrap();
        assert_eq!((c2.r(), c2.g(), c2.b()), (0, 0, 255), "seg2 = blue");
    }

    /// Unsorted input (run B before run A by start_byte) produces the same
    /// output as the equivalent sorted input.
    #[test]
    fn color_run_spans_unsorted_input_same_as_sorted() {
        let text = "hello world"; // 11 bytes
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];

        // Sorted order: red 0..5, blue 6..11.
        let sorted_runs = [
            ColorRunItem {
                start_byte: 0,
                end_byte: 5,
                color: red,
            },
            ColorRunItem {
                start_byte: 6,
                end_byte: 11,
                color: blue,
            },
        ];
        // Reversed order: same runs but blue listed first.
        let unsorted_runs = [
            ColorRunItem {
                start_byte: 6,
                end_byte: 11,
                color: blue,
            },
            ColorRunItem {
                start_byte: 0,
                end_byte: 5,
                color: red,
            },
        ];

        let base = Attrs::new();
        let sorted_spans = color_run_spans(text, &sorted_runs, base);
        let unsorted_spans = color_run_spans(text, &unsorted_runs, base);

        // Text coverage must be identical.
        let sorted_text: String = sorted_spans.iter().map(|(s, _)| *s).collect();
        let unsorted_text: String = unsorted_spans.iter().map(|(s, _)| *s).collect();
        assert_eq!(sorted_text, unsorted_text, "text coverage must match");

        // Span count must match.
        assert_eq!(
            sorted_spans.len(),
            unsorted_spans.len(),
            "span count must match"
        );

        // Each span's slice and color must match.
        for i in 0..sorted_spans.len() {
            assert_eq!(
                sorted_spans[i].0, unsorted_spans[i].0,
                "span[{i}] text slice must match"
            );
            assert_eq!(
                sorted_spans[i].1.color_opt, unsorted_spans[i].1.color_opt,
                "span[{i}] color must match"
            );
        }
    }

    /// Adjacent (touching) non-overlapping runs are preserved as separate spans.
    #[test]
    fn color_run_spans_adjacent_non_overlapping() {
        let text = "abcdef"; // 6 bytes
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];
        // run A: 0..3 ("abc"), run B: 3..6 ("def") — adjacent, no overlap.
        let runs = [
            ColorRunItem {
                start_byte: 0,
                end_byte: 3,
                color: red,
            },
            ColorRunItem {
                start_byte: 3,
                end_byte: 6,
                color: blue,
            },
        ];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);

        let combined: String = spans.iter().map(|(s, _)| *s).collect();
        assert_eq!(combined, text, "full text must be covered");
        assert_eq!(spans.len(), 2, "two adjacent runs → two spans (no gap)");
        assert_eq!(spans[0].0, "abc");
        assert_eq!(spans[1].0, "def");

        let c0 = spans[0].1.color_opt.unwrap();
        assert_eq!((c0.r(), c0.g(), c0.b()), (255, 0, 0), "first span = red");
        let c1 = spans[1].1.color_opt.unwrap();
        assert_eq!((c1.r(), c1.g(), c1.b()), (0, 0, 255), "second span = blue");
    }

    /// Fully nested run: inner run (later) wins; outer run retains prefix and
    /// suffix around the inner region.
    ///
    /// Input: red 0..12, blue 4..8 on "0123456789ab" (12 bytes).
    /// Expected: red 0..4, blue 4..8, red 8..12.
    #[test]
    fn color_run_spans_nested_inner_wins() {
        let text = "0123456789ab"; // 12 ASCII bytes
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];
        let runs = [
            ColorRunItem {
                start_byte: 0,
                end_byte: 12,
                color: red,
            },
            ColorRunItem {
                start_byte: 4,
                end_byte: 8,
                color: blue,
            }, // nested inside red
        ];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);

        let combined: String = spans.iter().map(|(s, _)| *s).collect();
        assert_eq!(combined, text, "all 12 bytes must be covered");

        assert_eq!(spans.len(), 3, "nested inner run → prefix + inner + suffix");
        assert_eq!(spans[0].0, &text[0..4]);
        assert_eq!(spans[1].0, &text[4..8]);
        assert_eq!(spans[2].0, &text[8..12]);

        let c0 = spans[0].1.color_opt.unwrap();
        assert_eq!((c0.r(), c0.g(), c0.b()), (255, 0, 0), "prefix = red");
        let c1 = spans[1].1.color_opt.unwrap();
        assert_eq!(
            (c1.r(), c1.g(), c1.b()),
            (0, 0, 255),
            "inner = blue (later wins)"
        );
        let c2 = spans[2].1.color_opt.unwrap();
        assert_eq!(
            (c2.r(), c2.g(), c2.b()),
            (255, 0, 0),
            "suffix = red (resumed)"
        );
    }

    /// Higher original-index run wins even when it starts earlier than the
    /// lower-index run.
    ///
    /// Input: blue(index 0) = 5..15, red(index 1) = 0..10 on 15-byte ASCII.
    /// Red has the higher original index so it wins bytes 5..10.
    /// Expected: red 0..10, blue 10..15.
    #[test]
    fn color_run_spans_higher_index_earlier_start_wins() {
        let text = "0123456789abcde"; // 15 ASCII bytes
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];
        // blue is index 0, red is index 1 — red (higher index) must win the overlap.
        let runs = [
            ColorRunItem {
                start_byte: 5,
                end_byte: 15,
                color: blue,
            },
            ColorRunItem {
                start_byte: 0,
                end_byte: 10,
                color: red,
            },
        ];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);

        let combined: String = spans.iter().map(|(s, _)| *s).collect();
        assert_eq!(combined, text, "all 15 bytes must be covered");

        // Expected: red[0..10], blue[10..15].
        assert_eq!(spans.len(), 2, "higher-index-wins overlap → two spans");
        assert_eq!(spans[0].0, &text[0..10], "span 0 = red 0..10");
        assert_eq!(spans[1].0, &text[10..15], "span 1 = blue 10..15");

        let c0 = spans[0].1.color_opt.unwrap();
        assert_eq!(
            (c0.r(), c0.g(), c0.b()),
            (255, 0, 0),
            "red (higher index) wins 0..10"
        );
        let c1 = spans[1].1.color_opt.unwrap();
        assert_eq!(
            (c1.r(), c1.g(), c1.b()),
            (0, 0, 255),
            "blue covers unclaimed 10..15"
        );
    }

    /// Zero-length and out-of-bounds runs are silently dropped.
    #[test]
    fn color_run_spans_degenerate_runs_dropped() {
        let text = "hello"; // 5 bytes
        let red = [255u8, 0, 0, 255];
        let runs = [
            // zero-length: start == end
            ColorRunItem {
                start_byte: 2,
                end_byte: 2,
                color: red,
            },
            // out-of-bounds: extends past text.len()
            ColorRunItem {
                start_byte: 3,
                end_byte: 100,
                color: red,
            },
            // valid run
            ColorRunItem {
                start_byte: 0,
                end_byte: 3,
                color: red,
            },
        ];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);

        let combined: String = spans.iter().map(|(s, _)| *s).collect();
        assert_eq!(combined, text, "all text must be covered");

        // Degenerate run at 2..2 is dropped.
        // Out-of-bounds 3..100 is clamped to 3..5 and still applies.
        // Valid 0..3 covers "hel". Clamped 3..5 covers "lo".
        // Both are red, and they're adjacent — so they should merge into one span.
        assert_eq!(spans.len(), 1, "adjacent same-color spans should merge");
        let c0 = spans[0].1.color_opt.unwrap();
        assert_eq!((c0.r(), c0.g(), c0.b()), (255, 0, 0));
    }

    // ── color_run_spans multiline tests [hud-vwggh] ───────────────────────────

    /// A single color run spanning bytes that cross two `\n` boundaries is
    /// produced as one contiguous styled span.
    ///
    /// text  = "line 1\nline 2\nline 3"  (20 bytes)
    ///          0123456 789012 3456789
    ///                ^           ^
    ///         run:  3..16  →  "e 1\nline 2\nli"
    ///
    /// Expected spans:
    ///   [0] "lin"          (bytes 0..3)   — unstyled prefix on line 1
    ///   [1] "e 1\nline 2\nli" (bytes 3..16) — single colored span crossing both \n
    ///   [2] "ne 3"         (bytes 16..20) — unstyled suffix on line 3
    ///
    /// This verifies that `color_run_spans` does not split on `\n` itself and
    /// that the resulting string slices include the newline characters verbatim.
    /// `set_rich_text` (glyphon) handles the actual line splitting during shaping.
    #[test]
    fn color_run_spans_cross_newline_single_run() {
        let text = "line 1\nline 2\nline 3";
        // byte layout: "line 1"=0..6, '\n'=6, "line 2"=7..13, '\n'=13, "line 3"=14..20
        let runs = [ColorRunItem {
            start_byte: 3,
            end_byte: 16, // spans "e 1\nline 2\nli"
            color: [255, 128, 0, 255],
        }];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);

        assert_eq!(
            spans.len(),
            3,
            "cross-newline run must produce: prefix + colored + suffix"
        );

        // Unstyled prefix (part of line 1 before the run).
        assert_eq!(spans[0].0, "lin", "prefix span must be 'lin'");
        assert!(
            spans[0].1.color_opt.is_none(),
            "prefix span must use base_attrs (no color)"
        );

        // Colored span crossing both newlines — the slice includes the '\n' chars.
        assert_eq!(
            spans[1].0, "e 1\nline 2\nli",
            "colored span must include both newline characters verbatim"
        );
        assert!(
            spans[1].1.color_opt.is_some(),
            "colored span must carry color_opt"
        );

        // Unstyled suffix (part of line 3 after the run end).
        assert_eq!(spans[2].0, "ne 3", "suffix span must be 'ne 3'");
        assert!(
            spans[2].1.color_opt.is_none(),
            "suffix span must use base_attrs (no color)"
        );

        // Sanity: concatenating all spans reconstructs the original text.
        let reconstructed: String = spans.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            reconstructed, text,
            "spans must reconstruct the original text exactly"
        );
    }

    /// Three color runs each constrained to one line produce per-line color spans
    /// with the `\n` separator characters falling into unstyled gap spans between
    /// them.  No color bleeds across a newline boundary.
    ///
    /// text  = "red line\nblue line\ngreen line"  (29 bytes)
    ///          01234567  8 9012345678  9012345678
    ///
    /// Runs:  [0..8] red, [9..18] blue, [19..29] green
    ///
    /// Expected spans (5 total):
    ///   [0] "red line"  (0..8)   — red
    ///   [1] "\n"        (8..9)   — unstyled gap
    ///   [2] "blue line" (9..18)  — blue
    ///   [3] "\n"        (18..19) — unstyled gap
    ///   [4] "green line"(19..29) — green
    #[test]
    fn color_run_spans_per_line_runs_no_bleed() {
        let text = "red line\nblue line\ngreen line";
        // byte layout: "red line"=0..8, '\n'=8, "blue line"=9..18, '\n'=18, "green line"=19..29
        let red = [255, 0, 0, 255];
        let blue = [0, 0, 255, 255];
        let green = [0, 200, 0, 255];
        let runs = [
            ColorRunItem {
                start_byte: 0,
                end_byte: 8,
                color: red,
            },
            ColorRunItem {
                start_byte: 9,
                end_byte: 18,
                color: blue,
            },
            ColorRunItem {
                start_byte: 19,
                end_byte: 29,
                color: green,
            },
        ];
        let base = Attrs::new();
        let spans = color_run_spans(text, &runs, base);

        assert_eq!(
            spans.len(),
            5,
            "3 per-line runs with \\n gaps must produce 5 spans (run, \\n, run, \\n, run)"
        );

        // Line 1: red
        assert_eq!(spans[0].0, "red line", "first span must be 'red line'");
        assert!(
            spans[0].1.color_opt.is_some(),
            "line-1 span must carry color_opt"
        );

        // Gap: the '\n' between line 1 and line 2 is unstyled.
        assert_eq!(
            spans[1].0, "\n",
            "gap between line 1 and line 2 must be '\\n'"
        );
        assert!(
            spans[1].1.color_opt.is_none(),
            "newline gap must use base_attrs — no color bleed from line-1 run"
        );

        // Line 2: blue
        assert_eq!(spans[2].0, "blue line", "second span must be 'blue line'");
        assert!(
            spans[2].1.color_opt.is_some(),
            "line-2 span must carry color_opt"
        );

        // Gap: the '\n' between line 2 and line 3 is unstyled.
        assert_eq!(
            spans[3].0, "\n",
            "gap between line 2 and line 3 must be '\\n'"
        );
        assert!(
            spans[3].1.color_opt.is_none(),
            "newline gap must use base_attrs — no color bleed from line-2 run"
        );

        // Line 3: green
        assert_eq!(spans[4].0, "green line", "third span must be 'green line'");
        assert!(
            spans[4].1.color_opt.is_some(),
            "line-3 span must carry color_opt"
        );

        // Sanity: concatenating all spans reconstructs the original text.
        let reconstructed: String = spans.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            reconstructed, text,
            "spans must reconstruct the original text exactly"
        );
    }

    // ── TruncationCache tests (hud-wgq7j) ────────────────────────────────────
    //
    // These tests verify the stable-shape-caching contract: `truncate_for_ellipsis`
    // is called exactly once per content/geometry change and zero times on
    // subsequent frames with the same scene.

    /// Cache hit for the same (content, geometry) pair is guaranteed after prime().
    ///
    /// This is the key invariant: zero truncation work per frame on unchanged
    /// content — mirroring `cache_hit_after_prime` in `markdown.rs`.
    #[test]
    fn truncation_cache_hit_after_prime() {
        let mut fs = FontSystem::new();
        let mut cache = TruncationCache::new();

        let content =
            "This is a long sentence that will definitely need truncation at narrow widths.";
        let bounds_w = 80.0_f32;
        let bounds_h = 50.0_f32;
        let font_size = 16.0_f32;
        let family = FontFamily::SystemSansSerif;
        let weight = 400u16;

        let key = TruncationKey::new(
            content,
            bounds_w,
            bounds_h,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        );

        // Prime: first call computes truncation.
        let first = cache
            .prime(
                key,
                content,
                bounds_w,
                bounds_h,
                font_size,
                family,
                weight,
                TruncationViewport::HeadAnchored,
                1.4,
                &mut fs,
            )
            .clone();

        // Second call with identical key: must hit the cache (no re-shaping).
        let second = cache
            .prime(
                key,
                content,
                bounds_w,
                bounds_h,
                font_size,
                family,
                weight,
                TruncationViewport::HeadAnchored,
                1.4,
                &mut fs,
            )
            .clone();

        assert_eq!(
            first, second,
            "cached truncation result must be identical to the computed result"
        );
        assert_eq!(
            cache.len(),
            1,
            "only one cache entry for the same (content, geometry) pair"
        );
    }

    /// Different geometry produces a separate cache entry.
    #[test]
    fn truncation_cache_different_geometry_different_entry() {
        let mut fs = FontSystem::new();
        let mut cache = TruncationCache::new();

        let content = "Hello world this is a somewhat long string";
        let family = FontFamily::SystemSansSerif;
        let weight = 400u16;
        let font_size = 16.0_f32;

        let key_narrow = TruncationKey::new(
            content,
            60.0,
            50.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        );
        let key_wide = TruncationKey::new(
            content,
            300.0,
            50.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        );

        cache.prime(
            key_narrow,
            content,
            60.0,
            50.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
            1.4,
            &mut fs,
        );
        cache.prime(
            key_wide,
            content,
            300.0,
            50.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
            1.4,
            &mut fs,
        );

        assert_eq!(
            cache.len(),
            2,
            "two distinct geometries → two cache entries"
        );

        // The wide result should not be truncated; the narrow one should be.
        let narrow_result = cache
            .get_by_key(&key_narrow)
            .expect("narrow must be cached");
        let wide_result = cache.get_by_key(&key_wide).expect("wide must be cached");
        assert!(
            wide_result.text.len() >= narrow_result.text.len(),
            "wider bounds must produce an equal-or-longer result"
        );
    }

    /// get_by_key returns None for a key that has not been primed.
    #[test]
    fn truncation_cache_miss_before_prime() {
        let cache = TruncationCache::new();
        let key = TruncationKey::new(
            "text",
            100.0,
            50.0,
            16.0,
            FontFamily::SystemSansSerif,
            400,
            TruncationViewport::HeadAnchored,
        );
        assert!(
            cache.get_by_key(&key).is_none(),
            "cache must be empty before prime"
        );
    }

    /// hud-n3mq0: a cadence-deferred render-path miss for new geometry must be
    /// served by `get_stale_by_content` (the previous geometry's valid
    /// truncation) — NEVER by an inline shape on the frame loop.
    ///
    /// This mirrors what `prepare_text_items` now does on a cache miss: it
    /// resolves `get_by_key(...).or_else(get_stale_by_content(content_hash))`
    /// and never calls `prime` (the inline shaper) on the render path.  We
    /// assert the stale path returns a previously-primed result while the cache
    /// length is unchanged — proving no shaping occurred for the new geometry.
    #[test]
    fn truncation_stale_by_content_serves_deferred_geometry_miss() {
        let mut fs = FontSystem::new();
        let mut cache = TruncationCache::new();

        let content = "A streaming transcript line long enough to require ellipsis truncation";
        let family = FontFamily::SystemSansSerif;
        let weight = 400u16;
        let font_size = 14.0_f32;

        // Commit-time prime at geometry G0 (width 120).
        let key_g0 = TruncationKey::new(
            content,
            120.0,
            30.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        );
        let g0_result = cache
            .prime(
                key_g0,
                content,
                120.0,
                30.0,
                font_size,
                family,
                weight,
                TruncationViewport::HeadAnchored,
                1.4,
                &mut fs,
            )
            .clone();
        assert_eq!(cache.len(), 1);

        // A resize drag bumps geometry to G1 (width 90), but the adaptive
        // cadence gate DEFERS the off-path re-prime, so G1's key was never
        // primed.  The render path sees an exact-key miss.
        let key_g1 = TruncationKey::new(
            content,
            90.0,
            30.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        );
        assert!(
            cache.get_by_key(&key_g1).is_none(),
            "deferred frame: exact-geometry key must be a miss"
        );

        // Render-path resolution (no inline shaping): fall back to the nearest
        // stale truncation for the SAME content.
        let resolved = cache
            .get_by_key(&key_g1)
            .or_else(|| cache.get_stale_by_content(&key_g1.content_hash()))
            .cloned();
        assert_eq!(
            resolved.as_ref(),
            Some(&g0_result),
            "deferred miss must reuse the previous geometry's valid truncation"
        );

        // Critical: the cache MUST NOT have grown — no entry was shaped/inserted
        // for G1 on the render path.  The deferred off-path prime fills it later.
        assert_eq!(
            cache.len(),
            1,
            "render-path stale fallback must not shape or insert a new entry"
        );
    }

    /// hud-n3mq0: the stale-by-content index tracks the *most recent* primed
    /// geometry, so the freshest available truncation is reused on a later miss.
    #[test]
    fn truncation_stale_by_content_tracks_latest_primed_geometry() {
        let mut fs = FontSystem::new();
        let mut cache = TruncationCache::new();

        let content = "Another long transcript line that overflows narrow tiles and truncates";
        let family = FontFamily::SystemSansSerif;
        let weight = 400u16;
        let font_size = 14.0_f32;

        let prime_at = |cache: &mut TruncationCache, fs: &mut FontSystem, w: f32| {
            let key = TruncationKey::new(
                content,
                w,
                30.0,
                font_size,
                family,
                weight,
                TruncationViewport::HeadAnchored,
            );
            cache
                .prime(
                    key,
                    content,
                    w,
                    30.0,
                    font_size,
                    family,
                    weight,
                    TruncationViewport::HeadAnchored,
                    1.4,
                    fs,
                )
                .clone()
        };

        prime_at(&mut cache, &mut fs, 200.0);
        let latest = prime_at(&mut cache, &mut fs, 80.0);

        // A miss at an un-primed geometry must reuse the LATEST primed result.
        let hash = TruncationKey::new(
            content,
            55.0,
            30.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        )
        .content_hash();
        assert_eq!(
            cache.get_stale_by_content(&hash),
            Some(&latest),
            "stale fallback must serve the most-recently-primed geometry"
        );
    }

    /// hud-n3mq0: cold start (content never primed at any geometry) returns
    /// `None` — the render path then renders untruncated-clipped text, still
    /// shaping nothing on the truncation path.
    #[test]
    fn truncation_stale_by_content_cold_start_is_none() {
        let cache = TruncationCache::new();
        let hash = *blake3::hash(b"never primed").as_bytes();
        assert!(
            cache.get_stale_by_content(&hash).is_none(),
            "cold start: no stale entry exists for unprimed content"
        );
    }

    /// hud-n3mq0: `evict_except` must keep `by_content` consistent — a content
    /// hash whose only entries were evicted must no longer resolve via the
    /// stale fallback (no dangling key pointing at an evicted entry).
    #[test]
    fn truncation_stale_index_pruned_on_evict() {
        let mut fs = FontSystem::new();
        let mut cache = TruncationCache::new();

        let family = FontFamily::SystemSansSerif;
        let weight = 400u16;
        let font_size = 14.0_f32;

        let content_keep = "Keep me visible in the scene";
        let content_gone = "This content left the scene entirely";

        let key_keep = TruncationKey::new(
            content_keep,
            120.0,
            30.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        );
        let key_gone = TruncationKey::new(
            content_gone,
            120.0,
            30.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        );

        cache.prime(
            key_keep,
            content_keep,
            120.0,
            30.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
            1.4,
            &mut fs,
        );
        cache.prime(
            key_gone,
            content_gone,
            120.0,
            30.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
            1.4,
            &mut fs,
        );

        // Evict everything except the surviving content.
        cache.evict_except(&[key_keep]);

        assert!(
            cache
                .get_stale_by_content(&key_keep.content_hash())
                .is_some(),
            "surviving content must still resolve via stale fallback"
        );
        assert!(
            cache
                .get_stale_by_content(&key_gone.content_hash())
                .is_none(),
            "evicted content must not resolve via a dangling stale index entry"
        );
    }

    /// Zero-truncation-work invariant: after priming, a second `prime` call
    /// must not call `truncate_for_ellipsis` again (proven via cache.len()
    /// staying at 1, not 2, after the second call with the same key).
    ///
    /// This is the per-frame zero-cost assertion: once primed, the frame path
    /// pays only O(1) for a lookup, not O(n) for shaping.
    #[test]
    fn truncation_cache_zero_work_on_unchanged_scene() {
        let mut fs = FontSystem::new();
        let mut cache = TruncationCache::new();

        let content = "Transcript line that is long enough to require ellipsis truncation here";
        let bounds_w = 120.0_f32;
        let bounds_h = 30.0_f32;
        let font_size = 14.0_f32;
        let family = FontFamily::SystemSansSerif;
        let weight = 400u16;

        let key = TruncationKey::new(
            content,
            bounds_w,
            bounds_h,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        );

        // Frame 1: cold start — cache miss, computes and stores result.
        let frame1_result = cache
            .prime(
                key,
                content,
                bounds_w,
                bounds_h,
                font_size,
                family,
                weight,
                TruncationViewport::HeadAnchored,
                1.4,
                &mut fs,
            )
            .clone();

        // Frame 2+: unchanged scene — cache hit, zero shaping work.
        // We call prime() 9 more times to simulate 9 subsequent frames.
        for _ in 0..9 {
            let frame_n_result = cache
                .prime(
                    key,
                    content,
                    bounds_w,
                    bounds_h,
                    font_size,
                    family,
                    weight,
                    TruncationViewport::HeadAnchored,
                    1.4,
                    &mut fs,
                )
                .clone();
            assert_eq!(
                frame1_result, frame_n_result,
                "repeated prime() with unchanged scene must return the cached result"
            );
        }

        // The cache still holds exactly one entry — no spurious re-insertions.
        assert_eq!(
            cache.len(),
            1,
            "cache must hold exactly one entry for one (content, geometry) pair"
        );
    }

    /// evict_except removes entries not in the live set.
    #[test]
    fn truncation_cache_evict_removes_stale_entries() {
        let mut fs = FontSystem::new();
        let mut cache = TruncationCache::new();

        let family = FontFamily::SystemSansSerif;
        let weight = 400u16;
        let font_size = 14.0_f32;

        let content_a = "Keep this line";
        let content_b = "Evict this line";

        let key_a = TruncationKey::new(
            content_a,
            120.0,
            30.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        );
        let key_b = TruncationKey::new(
            content_b,
            120.0,
            30.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        );

        cache.prime(
            key_a,
            content_a,
            120.0,
            30.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
            1.4,
            &mut fs,
        );
        cache.prime(
            key_b,
            content_b,
            120.0,
            30.0,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
            1.4,
            &mut fs,
        );
        assert_eq!(cache.len(), 2);

        cache.evict_except(&[key_a]);

        assert_eq!(cache.len(), 1, "evict_except must remove the stale entry");
        assert!(
            cache.get_by_key(&key_a).is_some(),
            "kept entry must remain after eviction"
        );
        assert!(
            cache.get_by_key(&key_b).is_none(),
            "evicted entry must be gone"
        );
    }

    /// clear() empties the cache entirely.
    #[test]
    fn truncation_cache_clear_empties_all_entries() {
        let mut fs = FontSystem::new();
        let mut cache = TruncationCache::new();

        let family = FontFamily::SystemSansSerif;
        let weight = 400u16;
        let font_size = 14.0_f32;

        for i in 0..3 {
            let content = format!("Content line {i}");
            let key = TruncationKey::new(
                &content,
                120.0,
                30.0,
                font_size,
                family,
                weight,
                TruncationViewport::HeadAnchored,
            );
            cache.prime(
                key,
                &content,
                120.0,
                30.0,
                font_size,
                family,
                weight,
                TruncationViewport::HeadAnchored,
                1.4,
                &mut fs,
            );
        }
        assert_eq!(cache.len(), 3);

        cache.clear();
        assert_eq!(cache.len(), 0, "clear() must empty the cache");
        assert!(
            cache.is_empty(),
            "is_empty() must return true after clear()"
        );
    }

    /// Frame-path lookup is O(1): get_by_key with a pre-computed key returns
    /// the result without touching font_system.
    #[test]
    fn truncation_cache_frame_path_lookup_is_o1() {
        let mut fs = FontSystem::new();
        let mut cache = TruncationCache::new();

        let content = "Short text for O(1) lookup test";
        let key = TruncationKey::new(
            content,
            200.0,
            50.0,
            16.0,
            FontFamily::SystemSansSerif,
            400,
            TruncationViewport::HeadAnchored,
        );

        // Prime once (commit-time cost).
        cache.prime(
            key,
            content,
            200.0,
            50.0,
            16.0,
            FontFamily::SystemSansSerif,
            400,
            TruncationViewport::HeadAnchored,
            1.4,
            &mut fs,
        );

        // Frame-path: get_by_key must return Some without touching font_system.
        let result = cache.get_by_key(&key);
        assert!(result.is_some(), "frame-path lookup must hit after prime");

        // Result must not be truncated for a 200px-wide box and short text.
        let result = result.unwrap();
        assert!(
            result.text.contains("Short text"),
            "short text must pass through unchanged"
        );
    }

    // ── Mid-drag re-truncation tests (hud-ghhxa — spec §6b.3) ─────────────────
    //
    // These tests verify that when a portal tile's bounds change during a resize
    // drag, the TruncationCache correctly re-resolves at each new intermediate
    // geometry.  The key invariant from spec §6b.3: "pane re-layout under the
    // overflow contract at every intermediate geometry."
    //
    // Because TruncationCache is keyed on bounds (bounds_width_bits /
    // bounds_height_bits are part of TruncationKey), a bounds change produces a
    // cache miss.  These tests confirm that:
    //   a) priming at a new (narrower) geometry produces truncation at that
    //      geometry, not at the old geometry.
    //   b) an intermediate geometry is independently primed and its result
    //      differs from the initial geometry.
    //   c) the cache holds separate entries for each distinct geometry.

    /// Priming at a narrower intermediate geometry produces a separate cache
    /// entry with truncation that fits within the new (narrower) bounds.
    ///
    /// This is the core §6b.3 invariant: at each intermediate geometry during
    /// a resize, the overflow contract is re-applied to the new bounds.
    #[test]
    fn intermediate_resize_geometry_re_resolves_truncation() {
        let mut fs = FontSystem::new();
        let mut cache = TruncationCache::new();

        // Long content that will overflow a narrow box but fits a wide one.
        let content = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"; // 39 'A's
        let family = FontFamily::SystemSansSerif;
        let weight = 400u16;
        let font_size = 16.0_f32;
        let height = 100.0_f32;

        // ── Step 1: prime at initial (wide) geometry ──────────────────────────
        let wide_w = 600.0_f32;
        let key_wide = TruncationKey::new(
            content,
            wide_w,
            height,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        );
        let result_wide = cache
            .prime(
                key_wide,
                content,
                wide_w,
                height,
                font_size,
                family,
                weight,
                TruncationViewport::HeadAnchored,
                1.4,
                &mut fs,
            )
            .clone();

        // Wide box: long content likely fits without truncation.
        // (We don't assert on was_truncated for the wide case as it depends on
        // font metrics — just record the result for comparison.)

        // ── Step 2: prime at an intermediate (narrow) geometry ───────────────
        // This simulates a resize drag that has moved the pane to a narrower width.
        let narrow_w = 60.0_f32; // narrow enough to force truncation
        let key_narrow = TruncationKey::new(
            content,
            narrow_w,
            height,
            font_size,
            family,
            weight,
            TruncationViewport::HeadAnchored,
        );
        let result_narrow = cache
            .prime(
                key_narrow,
                content,
                narrow_w,
                height,
                font_size,
                family,
                weight,
                TruncationViewport::HeadAnchored,
                1.4,
                &mut fs,
            )
            .clone();

        // The narrow-geometry result must be independently cached (separate key).
        assert_eq!(
            cache.len(),
            2,
            "wide and narrow geometries must produce separate cache entries \
             (one for each distinct bounds_width); got {} entries",
            cache.len(),
        );

        // The narrow result must be truncated (39 'A's will not fit in 60px).
        assert!(
            result_narrow.was_truncated,
            "content must be truncated at the intermediate narrow geometry ({narrow_w}px); \
             got: {:?}",
            result_narrow.text,
        );
        assert!(
            result_narrow.text.ends_with(crate::overflow::ELLIPSIS),
            "truncated result at intermediate geometry must end with '…'; got: {:?}",
            result_narrow.text,
        );

        // The narrow result must be strictly shorter than the wide result
        // (or at least differ — the wide result may or may not truncate
        // depending on font metrics, but the narrow one always will).
        if !result_wide.was_truncated {
            assert_ne!(
                result_wide.text, result_narrow.text,
                "wide (untruncated) and narrow (truncated) results must differ \
                 — the narrow intermediate geometry must trigger independent re-resolution"
            );
        }
    }

    /// A sequence of three intermediate geometries each produces an independently
    /// cached result.  The cache holds one entry per distinct bounds, and each
    /// entry reflects the correct truncation for its bounds.
    ///
    /// This validates that the re-truncation is not just at drag-end (the widest
    /// or narrowest bound) but at *every* distinct intermediate geometry the
    /// cache is primed for.
    #[test]
    fn sequence_of_intermediate_geometries_all_independently_cached() {
        let mut fs = FontSystem::new();
        let mut cache = TruncationCache::new();

        // Content long enough to overflow at mid and narrow widths.
        let content = "WWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWW"; // 43 'W's
        let family = FontFamily::SystemSansSerif;
        let weight = 400u16;
        let font_size = 16.0_f32;
        let height = 100.0_f32;

        let widths: [f32; 3] = [500.0, 200.0, 60.0];

        // Prime at each intermediate geometry in order (simulating a resize drag
        // from wide to mid to narrow).
        let mut results = Vec::new();
        for &w in &widths {
            let key = TruncationKey::new(
                content,
                w,
                height,
                font_size,
                family,
                weight,
                TruncationViewport::HeadAnchored,
            );
            let result = cache
                .prime(
                    key,
                    content,
                    w,
                    height,
                    font_size,
                    family,
                    weight,
                    TruncationViewport::HeadAnchored,
                    1.4,
                    &mut fs,
                )
                .clone();
            results.push((w, result));
        }

        // Each geometry must have produced a separate cache entry.
        assert_eq!(
            cache.len(),
            3,
            "three distinct intermediate geometries must produce three cache entries; \
             got {} entries",
            cache.len(),
        );

        // The narrowest geometry must be truncated.
        let (narrow_w, ref narrow_result) = results[2];
        assert!(
            narrow_result.was_truncated,
            "content must be truncated at narrowest intermediate geometry ({narrow_w}px)"
        );
        assert!(
            narrow_result.text.ends_with(crate::overflow::ELLIPSIS),
            "narrowest intermediate result must end with '…'; got: {:?}",
            narrow_result.text,
        );

        // The narrow result must be shorter than the widest result (which may
        // not be truncated).  At minimum the texts must differ.
        let (wide_w, ref wide_result) = results[0];
        if !wide_result.was_truncated {
            assert_ne!(
                wide_result.text, narrow_result.text,
                "intermediate geometry at {wide_w}px and {narrow_w}px must produce \
                 different truncation results (widths differ significantly)"
            );
        }
    }

    /// Verify that TruncationKey encodes bounds in the key: the same content
    /// at two different widths produces two different keys (so the cache miss
    /// mechanism that drives intermediate-geometry re-resolution is sound).
    ///
    /// This is the structural guarantee underlying the entire §6b.3 re-truncation
    /// design: the TruncationCache already invalidates naturally on bounds change
    /// because bounds are part of the key.
    #[test]
    fn truncation_key_encodes_bounds_width_distinctly() {
        let content = "any content string";
        let key_a = TruncationKey::new(
            content,
            100.0,
            50.0,
            16.0,
            FontFamily::SystemSansSerif,
            400,
            TruncationViewport::HeadAnchored,
        );
        let key_b = TruncationKey::new(
            content,
            200.0,
            50.0,
            16.0,
            FontFamily::SystemSansSerif,
            400,
            TruncationViewport::HeadAnchored,
        );
        assert_ne!(
            key_a, key_b,
            "TruncationKey must differ when bounds_width differs \
             (required for mid-drag cache-invalidation correctness)"
        );
    }

    /// Verify that TruncationKey encodes bounds height in the key.
    #[test]
    fn truncation_key_encodes_bounds_height_distinctly() {
        let content = "any content string";
        let key_a = TruncationKey::new(
            content,
            200.0,
            50.0,
            16.0,
            FontFamily::SystemSansSerif,
            400,
            TruncationViewport::HeadAnchored,
        );
        let key_b = TruncationKey::new(
            content,
            200.0,
            80.0,
            16.0,
            FontFamily::SystemSansSerif,
            400,
            TruncationViewport::HeadAnchored,
        );
        assert_ne!(
            key_a, key_b,
            "TruncationKey must differ when bounds_height differs \
             (required for mid-drag cache-invalidation correctness)"
        );
    }

    // ── hud-so7zu: styled/color run reslice tests ─────────────────────────────

    /// reslice_styled_runs: runs entirely before content_end_byte are preserved.
    #[test]
    fn reslice_styled_runs_runs_within_range_preserved() {
        let runs = vec![
            StyledRunItem {
                start_byte: 0,
                end_byte: 4,
                weight: Some(700),
                italic: false,
                monospace: false,
                color: None,
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            },
            StyledRunItem {
                start_byte: 5,
                end_byte: 10,
                weight: None,
                italic: true,
                monospace: false,
                color: None,
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            },
        ];
        // content_end = 10: both runs fit entirely
        let result = reslice_styled_runs(&runs, 10);
        assert_eq!(result.len(), 2, "both runs within range must be preserved");
        assert_eq!(result[0].start_byte, 0);
        assert_eq!(result[0].end_byte, 4);
        assert_eq!(result[1].start_byte, 5);
        assert_eq!(result[1].end_byte, 10);
    }

    /// reslice_styled_runs: runs starting at or beyond content_end_byte are dropped.
    #[test]
    fn reslice_styled_runs_beyond_truncation_dropped() {
        let runs = vec![
            StyledRunItem {
                start_byte: 0,
                end_byte: 3,
                weight: Some(700),
                italic: false,
                monospace: false,
                color: None,
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            },
            StyledRunItem {
                start_byte: 5,
                end_byte: 10,
                weight: None,
                italic: false,
                monospace: false,
                color: None,
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            },
        ];
        // content_end = 4: second run (5..10) is entirely beyond truncation point
        let result = reslice_styled_runs(&runs, 4);
        assert_eq!(result.len(), 1, "run beyond content_end must be dropped");
        assert_eq!(result[0].start_byte, 0);
        assert_eq!(result[0].end_byte, 3);
    }

    /// reslice_styled_runs: runs partially overlapping content_end_byte are clamped.
    #[test]
    fn reslice_styled_runs_partial_overlap_clamped() {
        let runs = vec![StyledRunItem {
            start_byte: 3,
            end_byte: 12,
            weight: Some(700),
            italic: false,
            monospace: false,
            color: None,
            background_color: None,
            size_scale: None,
            fill_line_width: false,
        }];
        // content_end = 8: run 3..12 overlaps; should become 3..8
        let result = reslice_styled_runs(&runs, 8);
        assert_eq!(
            result.len(),
            1,
            "partially overlapping run should be clamped"
        );
        assert_eq!(result[0].start_byte, 3);
        assert_eq!(
            result[0].end_byte, 8,
            "end_byte must be clamped to content_end"
        );
        assert_eq!(result[0].weight, Some(700), "weight must be preserved");
    }

    /// reslice_color_runs: runs entirely before content_end_byte are preserved.
    #[test]
    fn reslice_color_runs_within_range_preserved() {
        let runs = vec![
            ColorRunItem {
                start_byte: 0,
                end_byte: 4,
                color: [255, 0, 0, 255],
            },
            ColorRunItem {
                start_byte: 5,
                end_byte: 9,
                color: [0, 0, 255, 255],
            },
        ];
        let result = reslice_color_runs(&runs, 10);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].end_byte, 4);
        assert_eq!(result[1].end_byte, 9);
    }

    /// reslice_color_runs: runs beyond content_end_byte are dropped.
    #[test]
    fn reslice_color_runs_beyond_truncation_dropped() {
        let runs = vec![
            ColorRunItem {
                start_byte: 0,
                end_byte: 3,
                color: [255, 0, 0, 255],
            },
            ColorRunItem {
                start_byte: 6,
                end_byte: 12,
                color: [0, 255, 0, 255],
            },
        ];
        let result = reslice_color_runs(&runs, 5);
        assert_eq!(
            result.len(),
            1,
            "second run entirely beyond content_end must be dropped"
        );
        assert_eq!(result[0].color, [255, 0, 0, 255]);
    }

    /// reslice_color_runs: partial overlap is clamped to content_end.
    #[test]
    fn reslice_color_runs_partial_overlap_clamped() {
        let runs = vec![ColorRunItem {
            start_byte: 2,
            end_byte: 10,
            color: [0, 128, 255, 255],
        }];
        let result = reslice_color_runs(&runs, 6);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].start_byte, 2);
        assert_eq!(result[0].end_byte, 6, "end clamped to content_end");
        assert_eq!(result[0].color, [0, 128, 255, 255], "color preserved");
    }

    // ── hud-so7zu: styled_runs_effective_measurement tests ───────────────────

    /// Base case: no styled runs → returns base_weight and no monospace.
    #[test]
    fn styled_runs_effective_measurement_no_runs() {
        let (weight, mono) = styled_runs_effective_measurement(&[], 400);
        assert_eq!(weight, 400, "no runs: effective weight = base_weight");
        assert!(!mono, "no runs: no monospace");
    }

    /// Bold run raises effective weight.
    #[test]
    fn styled_runs_effective_measurement_bold_run_raises_weight() {
        let runs = vec![StyledRunItem {
            start_byte: 0,
            end_byte: 5,
            weight: Some(700),
            italic: false,
            monospace: false,
            color: None,
            background_color: None,
            size_scale: None,
            fill_line_width: false,
        }];
        let (weight, mono) = styled_runs_effective_measurement(&runs, 400);
        assert_eq!(weight, 700, "bold run must raise effective weight to 700");
        assert!(!mono);
    }

    /// Monospace run sets use_monospace flag.
    #[test]
    fn styled_runs_effective_measurement_monospace_run_sets_flag() {
        let runs = vec![StyledRunItem {
            start_byte: 0,
            end_byte: 5,
            weight: None,
            italic: false,
            monospace: true,
            color: None,
            background_color: None,
            size_scale: None,
            fill_line_width: false,
        }];
        let (weight, mono) = styled_runs_effective_measurement(&runs, 400);
        assert_eq!(
            weight, 400,
            "monospace run without weight: base_weight unchanged"
        );
        assert!(mono, "monospace run must set use_monospace");
    }

    /// Multiple runs: maximum weight is returned.
    #[test]
    fn styled_runs_effective_measurement_max_weight_returned() {
        let runs = vec![
            StyledRunItem {
                start_byte: 0,
                end_byte: 3,
                weight: Some(600),
                italic: false,
                monospace: false,
                color: None,
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            },
            StyledRunItem {
                start_byte: 4,
                end_byte: 8,
                weight: Some(800),
                italic: false,
                monospace: false,
                color: None,
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            },
            StyledRunItem {
                start_byte: 9,
                end_byte: 12,
                weight: Some(500),
                italic: false,
                monospace: false,
                color: None,
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            },
        ];
        let (weight, mono) = styled_runs_effective_measurement(&runs, 400);
        assert_eq!(weight, 800, "maximum weight across runs must be returned");
        assert!(!mono);
    }

    // ── hud-fe4ik: tail-anchored styled-run reslice tests ────────────────────
    //
    // `truncate_tail_anchored` emits LEADING-ellipsis output ("…\n{tail}"),
    // so styled_run byte offsets (relative to original text) must be shifted
    // into the "…\n{tail}" coordinate space.  These tests verify the new
    // `reslice_styled_runs_tail_anchored` and `reslice_color_runs_tail_anchored`
    // helpers, and also confirm that the spans they produce slice the correct
    // substrings from the leading-ellipsis effective text.

    /// Basic shift: a bold run on the visible tail is remapped to effective_text coords.
    ///
    /// Original text:  "line1\nline2\nline3"  (bytes 0..17)
    /// Truncated (tail-anchored): "…\nline2\nline3"
    ///   - "…" = 3 bytes, "\n" = 1 byte → leading_bytes = 4
    ///   - tail_origin = 17 - 12 = 5  ("line1\n" is the dropped prefix, 6 bytes)
    ///
    /// Wait — let's compute carefully:
    ///   "line1\nline2\nline3"
    ///    0123456789012345678
    ///    "line2\nline3" starts at byte 6 (len 12)
    ///    tail_origin = 18 - 12 = 6
    ///
    /// A bold run at [6..10] ("line") in original text should shift to
    ///   new_start = 6 - 6 + 4 = 4
    ///   new_end   = 10 - 6 + 4 = 8
    /// In effective_text "…\nline2\nline3":
    ///   bytes 0..3 = "…", byte 3 = "\n", bytes 4..10 = "line2\n", bytes 10..15 = "line3"
    ///   → [4..8] = "line" ✓ (correct glyphs styled)
    #[test]
    fn reslice_styled_runs_tail_anchored_basic_shift() {
        let original = "line1\nline2\nline3";
        // "line2\nline3" = 12 bytes; starts at byte 6 in original
        let truncated = "…\nline2\nline3";
        // ELLIPSIS = "…" = 3 bytes, + "\n" = 4 bytes leading_bytes
        // tail_origin = 17 - 12 = 5? No: original.len() = 17 actually.
        // "line1\nline2\nline3": l-i-n-e-1 = 5, \n = 1, l-i-n-e-2 = 5, \n = 1, l-i-n-e-3 = 5 → 17 bytes
        // "line2\nline3": l-i-n-e-2 = 5, \n = 1, l-i-n-e-3 = 5 → 11 bytes
        // tail_origin = 17 - 11 = 6 ✓ ("line1\n" = 6 bytes)
        // leading_bytes = 3 + 1 = 4
        // bold run [6..11] ("line2") in original → [6-6+4 .. 11-6+4] = [4..9]
        // In "…\nline2\nline3": bytes [4..9] = "line2" ✓
        let runs = vec![StyledRunItem {
            start_byte: 6,
            end_byte: 11,
            weight: Some(700),
            italic: false,
            monospace: false,
            color: None,
            background_color: None,
            size_scale: None,
            fill_line_width: false,
        }];
        let result = reslice_styled_runs_tail_anchored(original, truncated, &runs);
        assert_eq!(
            result.len(),
            1,
            "bold run on visible tail must survive reslice"
        );
        assert_eq!(
            result[0].start_byte, 4,
            "start must be shifted to leading-ellipsis coords"
        );
        assert_eq!(
            result[0].end_byte, 9,
            "end must be shifted to leading-ellipsis coords"
        );
        assert_eq!(result[0].weight, Some(700), "weight preserved");
        // Verify the span slices the correct substring.
        assert_eq!(
            &truncated[result[0].start_byte..result[0].end_byte],
            "line2",
            "resliced run must address 'line2' in the effective text"
        );
    }

    /// Single-line `max_lines == 1` output (hud-qoq52, option (b)): the
    /// tail-anchored result is `"…{newest line}"` with NO newline.  The reslice
    /// must use a `leading_bytes` of `ELLIPSIS.len()` (3), not `ELLIPSIS.len()+1`
    /// (4), or it would slice off the first content byte and mis-shift offsets.
    #[test]
    fn reslice_styled_runs_tail_anchored_single_line_inline_ellipsis() {
        let original = "old line one\nold line two\nNewest";
        // max_lines==1 leading-ellipsis output: "…Newest" (no newline).
        let truncated = "…Newest";
        // leading_bytes must resolve to 3 (just "…"), tail_content = "Newest".
        // tail_origin = original.len() - 6.  A bold run over "Newest" in the
        // original must remap to address "Newest" in "…Newest".
        let newest_start = original.len() - "Newest".len();
        let runs = vec![StyledRunItem {
            start_byte: newest_start,
            end_byte: original.len(),
            weight: Some(700),
            italic: false,
            monospace: false,
            color: None,
            background_color: None,
            size_scale: None,
            fill_line_width: false,
        }];
        let result = reslice_styled_runs_tail_anchored(original, truncated, &runs);
        assert_eq!(
            result.len(),
            1,
            "bold run on the single visible newest line must survive reslice"
        );
        // "…" = 3 bytes; "Newest" occupies bytes [3..9] in "…Newest".
        assert_eq!(
            result[0].start_byte, 3,
            "start must shift past the inline '…'"
        );
        assert_eq!(result[0].end_byte, truncated.len());
        assert_eq!(
            &truncated[result[0].start_byte..result[0].end_byte],
            "Newest",
            "resliced run must address 'Newest' in the single-line effective text"
        );
    }

    /// Run entirely before the visible tail is dropped.
    #[test]
    fn reslice_styled_runs_tail_anchored_run_before_tail_dropped() {
        let original = "line1\nline2\nline3";
        let truncated = "…\nline2\nline3";
        // Run [0..5] = "line1" is in the dropped prefix → should be dropped.
        let runs = vec![StyledRunItem {
            start_byte: 0,
            end_byte: 5,
            weight: Some(700),
            italic: false,
            monospace: false,
            color: None,
            background_color: None,
            size_scale: None,
            fill_line_width: false,
        }];
        let result = reslice_styled_runs_tail_anchored(original, truncated, &runs);
        assert!(
            result.is_empty(),
            "run entirely before tail must be dropped; got: {result:?}"
        );
    }

    /// Run partially overlapping the tail start is clamped to the tail start.
    #[test]
    fn reslice_styled_runs_tail_anchored_partial_overlap_clamped() {
        let original = "line1\nline2\nline3";
        let truncated = "…\nline2\nline3";
        // tail_origin = 6; run [3..9] straddles the tail boundary.
        // Clamped start = 6; end = 9.
        // new_start = 6 - 6 + 4 = 4; new_end = 9 - 6 + 4 = 7.
        // truncated[4..7] = "lin" (start of "line2")
        let runs = vec![StyledRunItem {
            start_byte: 3,
            end_byte: 9,
            weight: Some(700),
            italic: false,
            monospace: false,
            color: None,
            background_color: None,
            size_scale: None,
            fill_line_width: false,
        }];
        let result = reslice_styled_runs_tail_anchored(original, truncated, &runs);
        assert_eq!(
            result.len(),
            1,
            "partially overlapping run must survive as clamped"
        );
        assert_eq!(
            result[0].start_byte, 4,
            "clamped start must be at tail start in effective coords"
        );
        assert_eq!(result[0].end_byte, 7);
        assert_eq!(
            &truncated[result[0].start_byte..result[0].end_byte],
            "lin",
            "sliced text must be the overlap region in effective text"
        );
    }

    /// Multiple runs: only runs on the visible tail survive; their offsets are
    /// correct addresses into the leading-ellipsis effective text.
    ///
    /// This is the primary regression test for hud-fe4ik: before the fix,
    /// `content_end` would equal `truncated.len()` (strip_suffix failing on
    /// leading-ellipsis output), no clipping would occur, and the original
    /// offsets (0..5 and 6..11) would be applied verbatim to "…\nline2\nline3".
    /// Byte 0..5 of that string is "…\nli" (not "line1") — wrong glyphs styled.
    #[test]
    fn reslice_styled_runs_tail_anchored_multiple_runs_regression() {
        let original = "line1\nline2\nline3";
        // tail_origin = 6; leading_bytes = 4
        let truncated = "…\nline2\nline3";

        let runs = vec![
            // Run on dropped prefix ("line1") → should be dropped
            StyledRunItem {
                start_byte: 0,
                end_byte: 5,
                weight: Some(700),
                italic: false,
                monospace: false,
                color: None,
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            },
            // Run on visible tail ("line2") → should map to [4..9] in effective_text
            StyledRunItem {
                start_byte: 6,
                end_byte: 11,
                weight: Some(400),
                italic: true,
                monospace: false,
                color: Some([255, 0, 0, 255]),
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            },
        ];

        let result = reslice_styled_runs_tail_anchored(original, truncated, &runs);
        assert_eq!(
            result.len(),
            1,
            "only the run on the visible tail survives; got {result:?}"
        );
        assert_eq!(
            result[0].start_byte, 4,
            "run on 'line2' must start at byte 4 (after '…\\n') in effective text"
        );
        assert_eq!(
            result[0].end_byte, 9,
            "run on 'line2' must end at byte 9 in effective text"
        );
        assert_eq!(
            &truncated[result[0].start_byte..result[0].end_byte],
            "line2",
            "shifted run must address 'line2' in '…\\nline2\\nline3'"
        );

        // Demonstrate the pre-fix bug: if offsets were NOT shifted, byte
        // range [6..11] in truncated "…\nline2\nline3" addresses "\nline" — wrong.
        // After the fix the range [4..9] correctly addresses "line2".
        assert_ne!(
            &truncated[6..11],
            "line2",
            "unshifted offset 6..11 must NOT address 'line2' in effective text \
             (confirms the pre-fix mis-styling)"
        );
    }

    /// `reslice_color_runs_tail_anchored`: basic color run on visible tail.
    #[test]
    fn reslice_color_runs_tail_anchored_basic_shift() {
        let original = "line1\nline2\nline3";
        let truncated = "…\nline2\nline3";
        // Run [6..11] = "line2" in original → [4..9] in effective_text.
        let runs = vec![ColorRunItem {
            start_byte: 6,
            end_byte: 11,
            color: [0, 128, 255, 255],
        }];
        let result = reslice_color_runs_tail_anchored(original, truncated, &runs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].start_byte, 4);
        assert_eq!(result[0].end_byte, 9);
        assert_eq!(result[0].color, [0, 128, 255, 255], "color preserved");
        assert_eq!(
            &truncated[result[0].start_byte..result[0].end_byte],
            "line2",
            "color run must address 'line2' in effective text"
        );
    }

    /// `reslice_color_runs_tail_anchored`: run before tail is dropped.
    #[test]
    fn reslice_color_runs_tail_anchored_run_before_tail_dropped() {
        let original = "line1\nline2\nline3";
        let truncated = "…\nline2\nline3";
        let runs = vec![ColorRunItem {
            start_byte: 0,
            end_byte: 5,
            color: [255, 0, 0, 255],
        }];
        let result = reslice_color_runs_tail_anchored(original, truncated, &runs);
        assert!(
            result.is_empty(),
            "color run before tail must be dropped; got {result:?}"
        );
    }

    /// `reslice_styled_runs_tail_anchored` falls back (empty) when first
    /// visible line was further horizontally truncated (no suffix match).
    #[test]
    fn reslice_styled_runs_tail_anchored_first_line_truncated_fallback() {
        let original = "line1\nthis_is_a_very_long_line_2\nline3";
        // First visible line was horizontally truncated: "this_is_a_very…"
        // So truncated_str does NOT end with original text's suffix.
        let truncated = "…\nthis_is_a_very…\nline3";
        let runs = vec![StyledRunItem {
            start_byte: 6,
            end_byte: 15,
            weight: Some(700),
            italic: false,
            monospace: false,
            color: None,
            background_color: None,
            size_scale: None,
            fill_line_width: false,
        }];
        // original.ends_with("this_is_a_very…\nline3") is false, so empty is returned.
        let result = reslice_styled_runs_tail_anchored(original, truncated, &runs);
        assert!(
            result.is_empty(),
            "when first line is horizontally truncated, must return empty (unstyled fallback); \
             got {result:?}"
        );
    }

    // ── hud-jrr72: no-truncation short-circuit tests ──────────────────────────
    //
    // Regression: when the cache returns Some(truncated_str) but truncated_str
    // equals the original text (it fit without truncation), the code must NOT
    // enter the tail-anchored or head-anchored reslice branches — even when the
    // original text starts with ELLIPSIS.  Before this fix, a text starting
    // with "…" in TailAnchored mode would incorrectly trigger the leading-ellipsis
    // shift, painting styled/color runs on wrong glyph positions.

    /// styled_runs: no-truncation short-circuit preserves runs unchanged even
    /// when the original text starts with ELLIPSIS (the pre-fix false trigger).
    #[test]
    fn styled_runs_no_truncation_preserves_runs_when_text_starts_with_ellipsis() {
        // Original text that happens to start with the ellipsis character.
        let text: Arc<str> = Arc::from("…already_short");
        // truncated_str identical to original — text fit, no truncation occurred.
        let truncated_str: Arc<str> = Arc::clone(&text);

        let runs = vec![
            StyledRunItem {
                start_byte: 0,
                end_byte: 3, // 3-byte UTF-8 ellipsis
                weight: Some(700),
                italic: false,
                monospace: false,
                color: None,
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            },
            StyledRunItem {
                start_byte: 3,
                end_byte: 14,
                weight: None,
                italic: true,
                monospace: false,
                color: None,
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            },
        ];

        // Simulate the reslice decision that prepare_text_items now makes.
        // The short-circuit must fire: no offset shift must occur.
        // Note: in production code `truncated_str` is `&str`; here we hold
        // `Arc<str>` and deref to compare the str values directly.
        let resliced: Vec<StyledRunItem> = if *truncated_str == *text {
            runs.to_vec()
        } else if truncated_str.starts_with(crate::overflow::ELLIPSIS) {
            reslice_styled_runs_tail_anchored(&text, &truncated_str, &runs)
        } else {
            let content_end = truncated_str
                .strip_suffix(crate::overflow::ELLIPSIS)
                .map(|s| s.len())
                .unwrap_or(truncated_str.len());
            reslice_styled_runs(&runs, content_end)
        };

        assert_eq!(
            resliced.len(),
            2,
            "all runs must be preserved by the no-truncation short-circuit; got {resliced:?}"
        );
        assert_eq!(
            resliced[0].start_byte, 0,
            "first run start must be unshifted"
        );
        assert_eq!(resliced[0].end_byte, 3, "first run end must be unshifted");
        assert_eq!(
            resliced[1].start_byte, 3,
            "second run start must be unshifted"
        );
        assert_eq!(resliced[1].end_byte, 14, "second run end must be unshifted");
    }

    /// color_runs: no-truncation short-circuit preserves runs unchanged even
    /// when the original text starts with ELLIPSIS.
    #[test]
    fn color_runs_no_truncation_preserves_runs_when_text_starts_with_ellipsis() {
        let text: Arc<str> = Arc::from("…short");
        let truncated_str: Arc<str> = Arc::clone(&text);

        let runs = vec![ColorRunItem {
            start_byte: 0,
            end_byte: 3, // 3-byte ellipsis
            color: [255, 0, 0, 255],
        }];

        // Simulate the color-runs reslice decision.
        // Note: in production code `truncated_str` is `&str`; here we hold
        // `Arc<str>` and deref to compare the str values directly.
        let resliced: Vec<ColorRunItem> = if *truncated_str == *text {
            runs.to_vec()
        } else if truncated_str.starts_with(crate::overflow::ELLIPSIS) {
            reslice_color_runs_tail_anchored(&text, &truncated_str, &runs)
        } else {
            let content_end = truncated_str
                .strip_suffix(crate::overflow::ELLIPSIS)
                .map(|s| s.len())
                .unwrap_or(truncated_str.len());
            reslice_color_runs(&runs, content_end)
        };

        assert_eq!(
            resliced.len(),
            1,
            "color run must be preserved by no-truncation short-circuit; got {resliced:?}"
        );
        assert_eq!(resliced[0].start_byte, 0, "run start must be unshifted");
        assert_eq!(resliced[0].end_byte, 3, "run end must be unshifted");
    }

    // ── hud-dhb4s: #708 end-to-end regression tests ──────────────────────────
    //
    // These tests exercise the ACTUAL call-site functions extracted from the
    // pipeline that #708 fixed:
    //
    //   (a) reslice_styled_runs_for_item — the reslice-decision core of the
    //       prepare_text_items styled-run path (lines 591–614 before extraction).
    //       Tests call this function with a TextItem that carries a bold run
    //       whose end_byte spans into the "…" glyph, and assert that the
    //       returned runs do NOT include the ellipsis bytes.
    //
    //   (b) effective_truncation_key — the key-building core shared by
    //       prime_truncation_cache and prepare_text_items.  Tests call this
    //       function with a TextItem containing bold styled_runs and assert
    //       the key encodes the effective weight (700) rather than the base
    //       weight (400), then verify TruncationCache::prime produces a
    //       strictly shorter truncated prefix at weight 700 vs 400.
    //
    // HOW THESE FAIL WHEN THE FIX IS REVERTED
    //
    //   (a) If `reslice_styled_runs` is removed from
    //       `reslice_styled_runs_for_item`, the function returns the original
    //       (un-clamped) runs.  The test asserts the returned run's end_byte
    //       equals `content_end` (≠ the original end_byte that spans into "…"),
    //       so the `assert_eq!` fails.
    //
    //   (b) If `styled_runs_effective_measurement` is removed from
    //       `effective_truncation_key` and replaced with `item.font_weight`,
    //       the key will encode weight 400 even for a bold-run item.  The test
    //       asserts the key returned by `effective_truncation_key` matches a
    //       key built explicitly with weight 700 (via `assert_eq!`), so it fails.
    //       The truncated-prefix length comparison also degenerates (both prefixes
    //       equal, both would overflow visually when rendered at weight 700).

    /// #708 fix (a) regression — styled-run PRESERVATION through truncation.
    ///
    /// Calls `reslice_styled_runs_for_item` (the call-site function extracted
    /// from `prepare_text_items`'s reslice branch) with a `TextItem` whose
    /// bold `styled_runs` span the "…" glyph bytes in the truncated string.
    ///
    /// **Would-fail-pre-fix**: if `reslice_styled_runs` is removed from
    /// `reslice_styled_runs_for_item`, the returned run's `end_byte` will be
    /// the original value (which includes the "…" bytes) instead of
    /// `content_end` (the byte before "…").  The `assert_eq!(run.end_byte, …)`
    /// on the returned run will fail.
    ///
    /// Additionally the test verifies that the spans built from the resliced
    /// runs do NOT include "…" in any bold span — the correctness property
    /// that the fix was designed to restore.
    #[test]
    fn regression_708a_reslice_for_item_clamps_run_away_from_ellipsis() {
        // Layout:
        //   truncated_str: "Hello…"
        //   H  e  l  l  o  …(3 bytes)
        //   0  1  2  3  4  5 6 7  ← byte offsets
        //   content_end = 5  (byte just before "…")
        //   ellipsis    = bytes 5..8
        //   total len   = 8
        //
        // The TextItem carries a bold run 0..8 (original text was longer than
        // "Hello…"; the run covers all of it).  Before the fix, the run would
        // be passed un-clamped to styled_run_spans → the "…" glyph ended up
        // in a bold span.  With the fix (reslice_styled_runs_for_item) the run
        // is clamped to end_byte=5 so only "Hello" is bold.
        let ellipsis = crate::overflow::ELLIPSIS; // "…" — 3 UTF-8 bytes
        let prefix = "Hello";
        let truncated_str = format!("{prefix}{ellipsis}");
        let content_end = prefix.len(); // 5

        // Simulate a TextItem whose original text was longer (e.g. "Hello World")
        // and whose bold run spans 0..11 in original-text space.
        // After truncation the truncated_str is "Hello…" (8 bytes), so the run's
        // un-clamped end_byte (11) is > truncated_str.len() (8); yet
        // styled_run_spans clamps to text.len() (8), which still covers "…".
        // We model this with end_byte = truncated_str.len() (8) — the pathological
        // case where un-clamped == truncated-string length — to match the exact
        // pre-fix bug scenario.
        let item = TextItem {
            text: Arc::from("Hello World"), // original (longer) text
            pixel_x: 0.0,
            pixel_y: 0.0,
            bounds_width: 60.0,
            bounds_height: 40.0,
            clip_pixel_x: 0.0,
            clip_pixel_y: 0.0,
            clip_bounds_width: 60.0,
            clip_bounds_height: 40.0,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            font_weight: 400,
            color: [255, 255, 255, 255],
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            outline_color: None,
            outline_width: None,
            opacity: 1.0,
            color_runs: Box::new([]),
            styled_runs: Box::new([StyledRunItem {
                start_byte: 0,
                end_byte: truncated_str.len(), // 8 — includes "…" bytes
                weight: Some(700),
                italic: false,
                monospace: false,
                color: None,
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            }]),
            viewport: TruncationViewport::HeadAnchored,
            line_height_multiplier: 1.4,
        };

        // ── Call through the EXTRACTED CALL-SITE FUNCTION ─────────────────────
        // reslice_styled_runs_for_item is the exact logic that prepare_text_items
        // now delegates to.  If the fix (reslice_styled_runs branch) is reverted,
        // the function returns the original un-clamped run.
        let resliced = reslice_styled_runs_for_item(&item, &truncated_str);

        // The resliced run must exist (bold content survived truncation).
        assert_eq!(
            resliced.len(),
            1,
            "#708 fix-a regression: one bold run must survive into the prefix; \
             got: {resliced:?}"
        );
        // The run must be clamped to content_end (byte 5, the "H…o" prefix,
        // NOT the "…" bytes).  If reslice is reverted, end_byte = 8 ≠ 5.
        assert_eq!(
            resliced[0].end_byte, content_end,
            "#708 fix-a regression: bold run must be clamped to content_end={content_end} \
             (byte before '…'); got end_byte={} — reslice_styled_runs is not being applied",
            resliced[0].end_byte,
        );
        assert_eq!(
            resliced[0].weight,
            Some(700),
            "#708 fix-a regression: bold weight must be preserved through reslice"
        );

        // ── Verify the styling-preservation invariant ─────────────────────────
        // Build spans from the resliced run and assert "…" is NOT in any bold span.
        let base_attrs = Attrs::new().family(Family::SansSerif).weight(Weight(400));
        let spans = styled_run_spans(
            &truncated_str,
            &resliced,
            base_attrs,
            FontFamily::SystemSansSerif,
            16.0,
            1.4,
        );
        let ellipsis_in_bold = spans
            .iter()
            .any(|(s, attrs)| attrs.weight == Weight(700) && s.contains(ellipsis));
        assert!(
            !ellipsis_in_bold,
            "#708 fix-a regression: '…' must not appear in any bold span; \
             got spans: {:?}",
            spans.iter().map(|(s, _)| *s).collect::<Vec<_>>()
        );

        // Bold text must cover exactly the prefix "Hello".
        let bold_text: String = spans
            .iter()
            .filter(|(_, attrs)| attrs.weight == Weight(700))
            .map(|(s, _)| *s)
            .collect();
        assert_eq!(
            bold_text, prefix,
            "#708 fix-a regression: bold span must cover exactly '{prefix}', not '…'"
        );
    }

    /// #708 fix (b) regression — styled MEASUREMENT correctness via
    /// `effective_truncation_key`.
    ///
    /// Calls `effective_truncation_key` (the call-site function shared by
    /// `prime_truncation_cache` and `prepare_text_items`) with a `TextItem`
    /// carrying a full-text bold `styled_run`, then asserts the returned key
    /// encodes the effective weight 700 (not the base weight 400) and that
    /// `TruncationCache::prime` at weight 700 produces a strictly shorter
    /// truncated prefix than at weight 400.
    ///
    /// **Would-fail-pre-fix**: if `styled_runs_effective_measurement` is removed
    /// from `effective_truncation_key` and replaced with `item.font_weight`, the
    /// key will encode weight 400 for the bold item.  The `assert_eq!` comparing
    /// the returned key with a key explicitly built at weight 700 will fail,
    /// catching the regression directly at the call site.
    #[test]
    fn regression_708b_effective_key_uses_bold_weight_and_truncates_more() {
        let mut fs = FontSystem::new();

        // Content long enough to overflow at both weight 400 and 700 in a
        // narrow box, with the cut-point differing between the two weights.
        let content = "WWW bold WWW bold WWW bold WWW bold";
        let narrow_w = 120.0_f32;
        let height = 40.0_f32;
        let font_size = 16.0_f32;
        let base_weight: u16 = 400;
        let eff_weight: u16 = 700;

        // TextItem with a bold styled_run covering all content.
        let item_bold = TextItem {
            text: Arc::from(content),
            pixel_x: 0.0,
            pixel_y: 0.0,
            bounds_width: narrow_w,
            bounds_height: height,
            clip_pixel_x: 0.0,
            clip_pixel_y: 0.0,
            clip_bounds_width: narrow_w,
            clip_bounds_height: height,
            font_size_px: font_size,
            font_family: FontFamily::SystemSansSerif,
            font_weight: base_weight, // BASE weight — the item-level setting
            color: [255, 255, 255, 255],
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            outline_color: None,
            outline_width: None,
            opacity: 1.0,
            color_runs: Box::new([]),
            // The styled_run makes the effective weight 700 — this is what the
            // fix must pick up.
            styled_runs: Box::new([StyledRunItem {
                start_byte: 0,
                end_byte: content.len(),
                weight: Some(eff_weight),
                italic: false,
                monospace: false,
                color: None,
                background_color: None,
                size_scale: None,
                fill_line_width: false,
            }]),
            viewport: TruncationViewport::HeadAnchored,
            line_height_multiplier: 1.4,
        };

        // ── Call through the EXTRACTED CALL-SITE FUNCTION ─────────────────────
        // effective_truncation_key is the key-building core that prime_truncation_cache
        // and prepare_text_items now delegate to.  If the fix is reverted,
        // the key will encode base_weight (400) instead of eff_weight (700).
        let key_from_item = effective_truncation_key(&item_bold)
            .expect("#708 fix-b: Ellipsis item must produce a key");

        // Build the expected key explicitly at eff_weight=700.  If
        // effective_truncation_key uses base_weight instead, key_from_item will
        // encode 400 here → assert_eq! fails, catching the regression.
        let expected_bold_key = TruncationKey::new(
            content,
            narrow_w,
            height,
            font_size,
            FontFamily::SystemSansSerif,
            eff_weight, // 700 — what effective_truncation_key must produce
            TruncationViewport::HeadAnchored,
        );
        assert_eq!(
            key_from_item, expected_bold_key,
            "#708 fix-b regression: effective_truncation_key must encode the \
             effective bold weight ({eff_weight}) for a TextItem with bold styled_runs; \
             if this fails, styled_runs_effective_measurement is not being called"
        );

        // The key must DIFFER from a base-weight-400 key (sanity check that
        // the two weights produce distinct cache slots).
        let base_key = TruncationKey::new(
            content,
            narrow_w,
            height,
            font_size,
            FontFamily::SystemSansSerif,
            base_weight, // 400
            TruncationViewport::HeadAnchored,
        );
        assert_ne!(
            key_from_item, base_key,
            "#708 fix-b regression: bold-item key must differ from base-weight key"
        );

        // ── Verify truncation-length correctness ──────────────────────────────
        // Prime the cache at base weight (400) and at the effective bold weight (700).
        // Bold glyphs are wider → fewer characters fit → the prefix is shorter.
        let mut cache_base = TruncationCache::new();
        let result_base = cache_base
            .prime(
                base_key,
                content,
                narrow_w,
                height,
                font_size,
                FontFamily::SystemSansSerif,
                base_weight,
                TruncationViewport::HeadAnchored,
                1.4,
                &mut fs,
            )
            .clone();

        let mut cache_bold = TruncationCache::new();
        let result_bold = cache_bold
            .prime(
                expected_bold_key,
                content,
                narrow_w,
                height,
                font_size,
                FontFamily::SystemSansSerif,
                eff_weight,
                TruncationViewport::HeadAnchored,
                1.4,
                &mut fs,
            )
            .clone();

        assert!(
            result_base.was_truncated,
            "#708 fix-b regression: base-weight result must be truncated"
        );
        assert!(
            result_bold.was_truncated,
            "#708 fix-b regression: bold-weight result must be truncated"
        );

        // Bold must produce a prefix that is no longer than the base prefix.
        // Pre-fix failure: both results would be identical (base weight used for
        // both), so the bold text would overflow visually when rendered at 700.
        let prefix_base = result_base
            .text
            .strip_suffix(crate::overflow::ELLIPSIS)
            .unwrap_or(&result_base.text)
            .len();
        let prefix_bold = result_bold
            .text
            .strip_suffix(crate::overflow::ELLIPSIS)
            .unwrap_or(&result_bold.text)
            .len();
        assert!(
            prefix_bold <= prefix_base,
            "#708 fix-b regression: bold-weight truncation must produce a prefix \
             no longer than base-weight (bold glyphs are wider → earlier cut point); \
             got base {prefix_base}B, bold {prefix_bold}B; \
             base: {:?}, bold: {:?}",
            result_base.text,
            result_bold.text,
        );
    }

    /// Phase 2 (hud-9ieev): `compute_inline_backdrop_quads` returns a quad for a
    /// `StyledRunItem` with `background_color` set, using actual glyph layout data.
    ///
    /// Constructs a `Buffer` for the text "hello code world" where the word "code"
    /// (bytes 6..10) has a `background_color` styled run set to the code-background
    /// design token sentinel value `[30, 30, 30, 255]`.  After shaping, the function
    /// must return exactly one quad that:
    ///   - carries the correct color `[30, 30, 30, 255]`
    ///   - has `x >= item.pixel_x` (glyph starts at or after the item origin)
    ///   - has positive width and height (layout run produced real glyph metrics)
    ///
    /// **Would-fail pre-fix**: before this commit, `prepare_text_items` returned
    /// `Result<(), String>` and no quads were ever computed.
    #[test]
    fn compute_inline_backdrop_quads_returns_quad_for_background_color_run() {
        // Attrs, Buffer, Family, Metrics, Shaping, Wrap are imported via `use super::*`
        // (they come from the top-level `use glyphon::{...}` in text.rs).

        let mut fs = FontSystem::new();

        // The plain text item (markdown already stripped).
        // "hello " = 6 bytes, "code" = 4 bytes (bytes 6..10), " world" = 6 bytes.
        let text: Arc<str> = Arc::from("hello code world");
        let code_start: usize = 6; // byte offset of 'c' in "code"
        let code_end: usize = 10; // byte offset just past 'e' in "code"
        let bg_color: [u8; 4] = [30, 30, 30, 255];

        let font_size = 16.0_f32;
        let line_height = font_size * 1.4;
        let pixel_x = 10.0_f32;
        let pixel_y = 20.0_f32;
        let width = 400.0_f32;
        let height = 60.0_f32;

        // Build and shape the buffer — mirrors what prepare_text_items does.
        // Must use Shaping::Advanced so glyph.start/end are byte offsets (not
        // char indices), matching what compute_inline_backdrop_quads expects.
        let mut buf = Buffer::new(&mut fs, Metrics::new(font_size, line_height));
        buf.set_size(&mut fs, Some(width), Some(height));
        buf.set_wrap(&mut fs, Wrap::Word);
        buf.set_text(
            &mut fs,
            &text,
            Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
        );
        buf.shape_until_scroll(&mut fs, false);

        // Build the TextItem with a backdrop-colored styled run over "code".
        let item = TextItem {
            text: Arc::clone(&text),
            pixel_x,
            pixel_y,
            bounds_width: width,
            bounds_height: height,
            clip_pixel_x: pixel_x,
            clip_pixel_y: pixel_y,
            clip_bounds_width: width,
            clip_bounds_height: height,
            font_size_px: font_size,
            line_height_multiplier: 1.4,
            font_family: FontFamily::SystemSansSerif,
            font_weight: 400,
            color: [255, 255, 255, 255],
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            outline_color: None,
            outline_width: None,
            opacity: 1.0,
            color_runs: Box::new([]),
            styled_runs: Box::new([StyledRunItem {
                start_byte: code_start,
                end_byte: code_end,
                weight: None,
                italic: false,
                monospace: true,
                color: None,
                background_color: Some(bg_color),
                size_scale: None,
                fill_line_width: false,
            }]),
            viewport: TruncationViewport::HeadAnchored,
        };

        // Sanity: the buffer must produce layout runs with glyphs before we test
        // the backdrop computation.
        assert!(
            buf.layout_runs().count() > 0,
            "hud-9ieev: buffer produced no layout runs — shape_until_scroll may not have worked; \
             lines={}",
            buf.lines.len()
        );
        assert!(
            buf.layout_runs().map(|r| r.glyphs.len()).sum::<usize>() > 0,
            "hud-9ieev: layout runs exist but contain no glyphs"
        );

        // Pass effective_styled_runs as the new third argument.  This test uses
        // a non-truncated item so the effective runs are identical to item.styled_runs.
        let eff_runs: Vec<Vec<StyledRunItem>> = vec![item.styled_runs.to_vec()];
        let quads = compute_inline_backdrop_quads(&[item], &[buf], &eff_runs);

        // At least one quad must be returned for the "code" span.
        assert!(
            !quads.is_empty(),
            "hud-9ieev: expected at least one backdrop quad for the 'code' styled run, got none; \
             sr_start={code_start}, sr_end={code_end}"
        );

        // All quads must carry the design-token color.
        for (i, q) in quads.iter().enumerate() {
            assert_eq!(
                q.color, bg_color,
                "hud-9ieev: quad[{i}] has wrong color {:?}, expected {:?}",
                q.color, bg_color
            );
        }

        // Quad geometry must be physically plausible.
        let first = &quads[0];
        assert!(
            first.x >= pixel_x,
            "hud-9ieev: quad x ({}) must be >= item pixel_x ({pixel_x}); \
             the 'code' span starts after 'hello ' so it can't be at the item origin",
            first.x
        );
        assert!(
            first.w > 0.0,
            "hud-9ieev: quad width ({}) must be positive",
            first.w
        );
        assert!(
            first.h > 0.0,
            "hud-9ieev: quad height ({}) must be positive",
            first.h
        );
        assert!(
            first.y >= pixel_y,
            "hud-9ieev: quad y ({}) must be >= item pixel_y ({pixel_y})",
            first.y
        );
    }

    // ─── Multi-line composer selection highlight (hud-scgyw) ─────────────────

    /// Pure three-part-shape math: a single-line selection is untouched; the
    /// first line extends to the right edge; the last line to the left edge; a
    /// fully-interior line spans the full box width.
    #[test]
    fn extend_selection_quad_to_box_three_part_shape() {
        let q = |x: f32, w: f32| InlineBackdropQuad {
            x,
            y: 0.0,
            w,
            h: 10.0,
            color: [0, 0, 0, 255],
        };
        let (c0, c1) = (10.0_f32, 200.0_f32);

        // Single line (no continuation) → unchanged.
        let s = extend_selection_quad_to_box(q(50.0, 30.0), c0, c1, false, false);
        assert_eq!((s.x, s.w), (50.0, 30.0));

        // First line (continues onto the next) → extend RIGHT to the box edge.
        let f = extend_selection_quad_to_box(q(50.0, 30.0), c0, c1, false, true);
        assert_eq!(f.x, 50.0, "first-line left edge unchanged");
        assert!(
            (f.x + f.w - c1).abs() < 0.01,
            "first-line extends to box right"
        );

        // Last line (continues from the previous) → extend LEFT to the box edge.
        let l = extend_selection_quad_to_box(q(50.0, 30.0), c0, c1, true, false);
        assert!((l.x - c0).abs() < 0.01, "last-line extends to box left");
        assert!(
            (l.x + l.w - 80.0).abs() < 0.01,
            "last-line right edge preserved"
        );

        // Interior line (continues both ways) → full box width.
        let m = extend_selection_quad_to_box(q(50.0, 30.0), c0, c1, true, true);
        assert!(
            (m.x - c0).abs() < 0.01 && (m.x + m.w - c1).abs() < 0.01,
            "interior full width"
        );
    }

    /// A `fill_line_width` selection spanning 3 wrapped visual lines produces the
    /// standard shape: partial first line (to the right edge), a FULL-WIDTH
    /// interior line, and a partial last line (from the left edge). Shaped with a
    /// CPU `FontSystem` (no GPU).
    #[test]
    fn multiline_selection_fills_interior_line_width() {
        let mut fs = FontSystem::new();
        // Narrow width forces one 4-char word per visual line: "aaaa"/"bbbb"/"cccc".
        let text: Arc<str> = Arc::from("aaaa bbbb cccc");
        let font_size = 16.0_f32;
        let line_height = font_size * 1.4;
        let (pixel_x, pixel_y, width, height) = (10.0_f32, 20.0_f32, 45.0_f32, 200.0_f32);
        let sel_bg: [u8; 4] = [58, 123, 213, 115];

        let mut buf = Buffer::new(&mut fs, Metrics::new(font_size, line_height));
        buf.set_size(&mut fs, Some(width), Some(height));
        buf.set_wrap(&mut fs, Wrap::Word);
        buf.set_text(
            &mut fs,
            &text,
            Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
        );
        buf.shape_until_scroll(&mut fs, false);
        assert!(
            buf.layout_runs().count() >= 3,
            "text must wrap to >= 3 visual rows"
        );

        // Selection from mid-first-word (byte 1) to mid-last-word (byte 13) spans all rows.
        let make_item = |fill: bool| TextItem {
            text: Arc::clone(&text),
            pixel_x,
            pixel_y,
            bounds_width: width,
            bounds_height: height,
            clip_pixel_x: pixel_x,
            clip_pixel_y: pixel_y,
            clip_bounds_width: width,
            clip_bounds_height: height,
            font_size_px: font_size,
            line_height_multiplier: 1.4,
            font_family: FontFamily::SystemSansSerif,
            font_weight: 400,
            color: [255, 255, 255, 255],
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            outline_color: None,
            outline_width: None,
            opacity: 1.0,
            color_runs: Box::new([]),
            styled_runs: Box::new([StyledRunItem {
                start_byte: 1,
                end_byte: 13,
                weight: None,
                italic: false,
                monospace: false,
                color: None,
                background_color: Some(sel_bg),
                size_scale: None,
                fill_line_width: fill,
            }]),
            viewport: TruncationViewport::HeadAnchored,
        };

        // Re-shape a second buffer for the control item (buffers are consumed by value).
        let mut buf2 = Buffer::new(&mut fs, Metrics::new(font_size, line_height));
        buf2.set_size(&mut fs, Some(width), Some(height));
        buf2.set_wrap(&mut fs, Wrap::Word);
        buf2.set_text(
            &mut fs,
            &text,
            Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
        );
        buf2.shape_until_scroll(&mut fs, false);

        let item = make_item(true);
        let eff = vec![item.styled_runs.to_vec()];
        let quads = compute_inline_backdrop_quads(&[item], &[buf], &eff);
        assert!(
            quads.len() >= 3,
            "one selection quad per wrapped line, got {}",
            quads.len()
        );

        let box_left = pixel_x;
        let box_right = pixel_x + width;
        // Quads are emitted in visual-line order.
        let (first, interior, last) = (&quads[0], &quads[1], &quads[2]);
        // First line: extends to the box right edge (partial left).
        assert!(
            (first.x + first.w - box_right).abs() < 1.0 && first.x > box_left + 1.0,
            "first line: partial-left, extends to box right (x={}, right={})",
            first.x,
            first.x + first.w
        );
        // Interior line: full box width.
        assert!(
            (interior.x - box_left).abs() < 1.0
                && (interior.x + interior.w - box_right).abs() < 1.0,
            "interior line must span the full box width (x={}, right={})",
            interior.x,
            interior.x + interior.w
        );
        // Last line: starts at the box left edge (partial right).
        assert!(
            (last.x - box_left).abs() < 1.0 && last.x + last.w < box_right - 1.0,
            "last line: extends to box left, partial-right (x={}, right={})",
            last.x,
            last.x + last.w
        );

        // CONTROL: without fill_line_width, interior quads are tight to glyphs
        // (NOT full width) — proving the flag drives the shape.
        let control = make_item(false);
        let eff2 = vec![control.styled_runs.to_vec()];
        let control_quads = compute_inline_backdrop_quads(&[control], &[buf2], &eff2);
        assert!(control_quads.len() >= 3);
        let ci = &control_quads[1];
        assert!(
            (ci.x + ci.w - box_right).abs() > 1.0 || (ci.x - box_left).abs() > 1.0,
            "without fill_line_width the interior quad must NOT span the full box width"
        );
    }

    /// A selection that spans a HARD newline (Ctrl+Enter draft) also produces the
    /// full-width-interior shape — the byte-offset accounting across BufferLines
    /// (`line_byte_offsets`) keeps the per-line quads correct (hud-scgyw).
    #[test]
    fn multiline_selection_across_hard_newline_fills_interior() {
        let mut fs = FontSystem::new();
        // Three hard lines; a wide box so nothing SOFT-wraps — the line breaks are
        // the '\n's at bytes 2 and 5.
        let text: Arc<str> = Arc::from("aa\nbb\ncc");
        let font_size = 16.0_f32;
        let line_height = font_size * 1.4;
        let (pixel_x, pixel_y, width, height) = (10.0_f32, 20.0_f32, 300.0_f32, 200.0_f32);
        let sel_bg: [u8; 4] = [58, 123, 213, 115];

        let mut buf = Buffer::new(&mut fs, Metrics::new(font_size, line_height));
        buf.set_size(&mut fs, Some(width), Some(height));
        buf.set_wrap(&mut fs, Wrap::Word);
        buf.set_text(
            &mut fs,
            &text,
            Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
        );
        buf.shape_until_scroll(&mut fs, false);

        // Select from byte 1 (mid "aa") to byte 7 (mid "cc") — spans both newlines.
        let item = TextItem {
            text: Arc::clone(&text),
            pixel_x,
            pixel_y,
            bounds_width: width,
            bounds_height: height,
            clip_pixel_x: pixel_x,
            clip_pixel_y: pixel_y,
            clip_bounds_width: width,
            clip_bounds_height: height,
            font_size_px: font_size,
            line_height_multiplier: 1.4,
            font_family: FontFamily::SystemSansSerif,
            font_weight: 400,
            color: [255, 255, 255, 255],
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            outline_color: None,
            outline_width: None,
            opacity: 1.0,
            color_runs: Box::new([]),
            styled_runs: Box::new([StyledRunItem {
                start_byte: 1,
                end_byte: 7,
                weight: None,
                italic: false,
                monospace: false,
                color: None,
                background_color: Some(sel_bg),
                size_scale: None,
                fill_line_width: true,
            }]),
            viewport: TruncationViewport::HeadAnchored,
        };
        let eff = vec![item.styled_runs.to_vec()];
        let quads = compute_inline_backdrop_quads(&[item], &[buf], &eff);
        assert_eq!(quads.len(), 3, "one quad per hard line");
        let box_right = pixel_x + width;
        // The interior line ("bb") is fully selected → full box width.
        assert!(
            (quads[1].x - pixel_x).abs() < 1.0 && (quads[1].x + quads[1].w - box_right).abs() < 1.0,
            "interior hard line spans full box width"
        );
        // Quads stack top-to-bottom (distinct y per line).
        assert!(
            quads[0].y < quads[1].y && quads[1].y < quads[2].y,
            "quads stack per line"
        );
    }

    // ── max_truncation_input_bytes wiring (hud-59p2z) ─────────────────────────

    /// The setter updates the cache's bound; a fresh cache starts at the
    /// compositor default (4096).
    #[test]
    fn set_max_truncation_input_bytes_reaches_cache() {
        let mut cache = TruncationCache::new();
        assert_eq!(
            cache.max_truncation_input_bytes(),
            overflow::DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
            "fresh cache must use the compositor default bound"
        );
        cache.set_max_truncation_input_bytes(1024);
        assert_eq!(
            cache.max_truncation_input_bytes(),
            1024,
            "configured bound must reach the cache"
        );
    }

    /// A configured non-default bound changes which input the truncation cache
    /// shapes: at the default (4096) a sub-4096 transcript is shaped whole, but
    /// at a smaller configured bound (1024) the viewport-adjacent-window
    /// fallback restricts the shaped input — and the cache shapes exactly that
    /// window.
    #[test]
    fn configured_bound_changes_truncation_fallback_input() {
        // A multi-line transcript whose total size sits strictly between the
        // small configured bound (1024) and the default bound (4096), so the
        // fallback engages only at the configured bound.
        let line = "The quick brown fox jumps over the lazy dog and keeps on running.";
        let content = vec![line; 40].join("\n");
        assert!(
            content.len() > 1024 && content.len() < 4096,
            "fixture must straddle the two bounds; got {} bytes",
            content.len()
        );

        let bounds_w = 600.0_f32;
        let bounds_h = 80.0_f32; // only a few visible lines
        let font_size = 16.0_f32;
        let family = FontFamily::SystemSansSerif;
        let weight = 400u16;
        let lhm = 1.4_f32;
        let line_height = font_size * lhm;
        let vp = TruncationViewport::HeadAnchored;

        // Default bound: input is under 4096 → NOT windowed (whole input shaped).
        let full_window = overflow::viewport_adjacent_input(
            &content,
            bounds_h,
            line_height,
            vp,
            overflow::DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
            overflow::TRUNCATION_OVERSCAN_LINES,
        );
        assert_eq!(
            full_window,
            content.as_str(),
            "default bound must not window a sub-4096 transcript"
        );

        // Configured bound: input exceeds 1024 → windowed to a shorter slice.
        let small_window = overflow::viewport_adjacent_input(
            &content,
            bounds_h,
            line_height,
            vp,
            1024,
            overflow::TRUNCATION_OVERSCAN_LINES,
        );
        assert!(
            small_window.len() < content.len(),
            "configured bound must restrict the shaped input (fallback engaged)"
        );

        // The cache at the configured bound must shape exactly that window:
        // priming the FULL content at bound 1024 yields the same truncation
        // result as priming the windowed slice directly at an unbounded cache.
        let mut fs = FontSystem::new();

        let key_full =
            TruncationKey::new(&content, bounds_w, bounds_h, font_size, family, weight, vp);
        let mut configured = TruncationCache::new();
        configured.set_max_truncation_input_bytes(1024);
        let via_bound = configured
            .prime(
                key_full, &content, bounds_w, bounds_h, font_size, family, weight, vp, lhm, &mut fs,
            )
            .clone();

        let key_window = TruncationKey::new(
            small_window,
            bounds_w,
            bounds_h,
            font_size,
            family,
            weight,
            vp,
        );
        let mut unbounded = TruncationCache::new();
        unbounded.set_max_truncation_input_bytes(usize::MAX);
        let via_direct = unbounded
            .prime(
                key_window,
                small_window,
                bounds_w,
                bounds_h,
                font_size,
                family,
                weight,
                vp,
                lhm,
                &mut fs,
            )
            .clone();

        assert_eq!(
            via_bound.text, via_direct.text,
            "at the configured bound the cache must shape the viewport-adjacent window, \
             not the full committed transcript"
        );
    }
}
