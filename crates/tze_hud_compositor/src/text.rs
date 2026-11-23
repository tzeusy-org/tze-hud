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
//!   included in the shaped-width budget, implemented in [`crate::overflow`].
//!   Grapheme-cluster fallback applies when no word boundary
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

use crate::fonts::{FontConfig, ResolvedFonts, build_font_system_with_diagnostics};
use crate::overflow::{self, TruncationResult, TruncationViewport};

/// Default line-height multiplier: `line_height_px = font_size_px × this`.
///
/// This is the single source of truth for the compositor's default line height,
/// used by [`crate::markdown::MarkdownTokens::default`] and any render path that
/// lacks a parsed `MarkdownTokens` (e.g. zone items, the legacy `TextItem`
/// constructor). A design-token map may override it per-tile via
/// `typography.line_height.multiplier`.
pub(crate) const LINE_HEIGHT_MULTIPLIER: f32 = tze_hud_scene::types::TEXT_LINE_HEIGHT_MULTIPLIER;

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
/// depending on which anchor mode is active.
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
/// Truncation is called only when content or geometry
/// changes — never on every frame.
///
/// Mirrors [`crate::markdown::MarkdownCache`] in structure and ownership model.
///
/// # Stale-by-content fallback
///
/// The exact-key entries above are only ever populated by [`prime`], which runs
/// off the render path (commit-time / cadence-gated geometry change).  During a
/// fast resize drag the adaptive cadence gate *defers* re-priming, so the
/// render path can see a cache miss for the newest geometry's key.  Rather than
/// shaping inline on the frame loop (the exact O(n) cost the cache exists to
/// avoid), the render path falls back to the most-recently-primed
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
    /// truncation fallback. When a single uncached
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
    #[cfg(test)]
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
    /// not yet primed (a cadence-deferred frame — ).
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
    ///   scrolled-back viewports.
    ///
    /// - [`TruncationViewport::TailAnchored`] — calls
    ///   [`overflow::truncate_tail_anchored`].  Used for follow-tail streaming
    ///   transcripts whose viewport is at the tail.
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
        // geometry's truncation instead of shaping inline. Updated
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
            // Viewport-adjacent-window fallback: when the
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
/// Created once per `Compositor` via [`TextRasterizer::with_font_config`]. Holds the
/// font system, glyph atlas, and renderer. Not `Send` — must stay on the
/// compositor thread.
pub(crate) struct TextRasterizer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    renderer: TextRenderer,
    /// Second glyphon renderer sharing `atlas`, prepared and replayed AFTER every
    /// other pass so runtime chrome text (the system card) cannot be covered by
    /// content, widgets, focus rings, menus or the safe-mode overlay.
    overlay_renderer: TextRenderer,
    /// Grip menu text, replayed after its plate and before final system chrome.
    menu_renderer: TextRenderer,
    affordance_renderer: TextRenderer,
    /// Content-addressed IDs of agent-uploaded fonts already loaded into
    /// `font_system`.  Used to skip redundant `load_font_data` calls (which
    /// would add duplicate entries to fontdb).
    ///
    /// The ID is the raw 32-byte BLAKE3 digest (`ResourceId` wire form) of
    /// the font bytes, matching the key used by `tze_hud_resource::FontBytesStore`.
    loaded_font_ids: HashSet<[u8; 32]>,
    /// Bytes of every agent-uploaded font, in load order, so a rasterizer
    /// rebuilt for a new [`FontConfig`] can carry them over
    /// ([`TextRasterizer::take_uploaded_fonts`]).  Shared with fontdb (no copy).
    uploaded_fonts: Vec<([u8; 32], Arc<Vec<u8>>)>,
    /// Family each font role resolved to.
    resolved_fonts: ResolvedFonts,
    font_warnings: tze_hud_scene::startup_diagnostics::StartupWarnings,
    /// Truncation result cache for `TextOverflow::Ellipsis` items.
    ///
    /// Keyed on `(content_hash, bounds_width, bounds_height, font_size_px,
    /// font_family, font_weight)`.  Populated outside the frame loop by
    /// [`TextRasterizer::prime_truncation_cache`]; the frame path looks up
    /// pre-computed results in O(1) via [`TruncationCache::get_by_key`].
    ///
    /// `truncate_for_ellipsis` is called exactly once per
    /// content/geometry change, never on every frame.
    pub(crate) truncation_cache: TruncationCache,
    /// Cumulative count of `shape_until_scroll` invocations on the per-frame
    /// `prepare_text_items` path.
    ///
    /// This is the measurement lever for the scroll-flicker fix: a `Buffer` is
    /// shaped only when its content/geometry/font changed, so on a pure scroll
    /// (offset changes but shape inputs do not) this counter must stay flat once
    /// a shaped-buffer cache lands. Incremented immediately before each frame-path
    /// `shape_until_scroll`; read via [`TextRasterizer::shape_call_count`].
    #[cfg(any(test, feature = "dev-mode"))]
    pub(crate) shape_call_count: u64,
    /// Shaped-buffer reuse cache: owned glyphon `Buffer`s keyed by a
    /// content-and-geometry `ShapeKey` (BLAKE3 of every input that affects the
    /// shaped glyphs — see [`shape_key_for_item`]). The **scroll offset is
    /// deliberately NOT in the key** — a Clip pane's shaped output is
    /// offset-independent (offset only moves the draw position), so a pure scroll
    /// reuses the shaped buffer with zero re-shape (the whole win). Content /
    /// bounds / font / run change → key miss → exactly one re-shape.
    ///
    /// Bounded by construction: `prepare_text_items` moves the whole map out at
    /// the top of the frame and rebuilds it from *this frame's* live items, so it
    /// retains one owned buffer per item in the latest actually prepared set,
    /// including duplicate keys. Unused old keys and surplus buffers are dropped.
    /// A skipped empty layer keeps its last prepared set without drawing it.
    /// Front-runs the Wave-3 "perf-whole-scene-reshape-per-frame"
    /// shaped-buffer-reuse item.
    shape_cache: HashMap<[u8; 32], Vec<Buffer>>,
    /// Shaped-buffer cache for the overlay layer, kept apart so a frame that
    /// prepares overlay text does not evict the content layer's buffers.
    overlay_shape_cache: HashMap<[u8; 32], Vec<Buffer>>,
    menu_shape_cache: HashMap<[u8; 32], Vec<Buffer>>,
    affordance_shape_cache: HashMap<[u8; 32], Vec<Buffer>>,
    /// Lazy system fallback faces for glyphs no loaded face covers
    /// (hud-ljhku.13, [`crate::fonts::SystemFallback`]).
    system_fallback: crate::fonts::SystemFallback,
}

/// Which glyphon renderer (and shaped-buffer cache) a prepare call targets.
#[derive(Clone, Copy)]
enum TextLayer {
    /// Scene content text, drawn in the main text pass.
    Content,
    /// Runtime chrome text, drawn in the final pass above everything else.
    Overlay,
    Menu,
    Affordance,
}

/// Wrap policy for wrapped text (composer draft and every
/// bounded `TextItem` in the render path).
///
/// [`Wrap::WordOrGlyph`] wraps at word boundaries **and** falls back to breaking
/// a single token at the glyph level when that token is wider than the line box.
/// Plain [`Wrap::Word`] would leave such an unbreakable token on one
/// over-long line whose tail is then clipped away by the box scissor — the owner
/// could type a 200-char run of `asdasd…` and never see the end. `WordOrGlyph` is
/// a strict superset of `Word` (identical for text that already fits), so this is
/// safe to apply to the shared render path.
///
/// This constant is the single source of truth for that policy: the measurement
/// helpers ([`TextRasterizer::measure_composer_wrapped`],
/// [`TextRasterizer::measure_composer_visual_layout`]) and the render path
/// ([`TextRasterizer::build_and_shape_buffer`]) all use it, so predicted line
/// counts and painted line counts always agree (no caret drift).
pub(crate) const WRAPPED_TEXT_WRAP: Wrap = Wrap::WordOrGlyph;

/// Shape `text` wrapped to `wrap_width` under [`WRAPPED_TEXT_WRAP`] and return the
/// advance width (`line_w`) of each resulting visual line, top-to-bottom.
///
/// Pure CPU helper for the existing break-anywhere wrap regressions. It uses
/// the same sans-serif attributes and metrics as the live composer path.
#[cfg(test)]
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

/// Advance width (px) of `text` shaped on one unwrapped line, as the overlay
/// text layer draws it. Runtime chrome (the control panel) uses it to size
/// pills and right-aligned values exactly.
pub(crate) fn single_line_width(
    font_system: &mut FontSystem,
    text: &str,
    font_size_px: f32,
    font_family: FontFamily,
    font_weight: u16,
) -> f32 {
    let mut buf = Buffer::new(font_system, Metrics::new(font_size_px, font_size_px * 1.4));
    buf.set_size(font_system, None, None);
    let family = match font_family {
        FontFamily::SystemSansSerif => Family::SansSerif,
        FontFamily::SystemMonospace => Family::Monospace,
        FontFamily::SystemSerif => Family::Serif,
    };
    let attrs = Attrs::new().family(family).weight(Weight(font_weight));
    buf.set_text(font_system, text, attrs, Shaping::Advanced);
    buf.shape_until_scroll(font_system, false);
    buf.layout_runs().map(|run| run.line_w).fold(0.0, f32::max)
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
/// padding from the same formula so their measurements cannot drift.
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
/// than a second, independently-drifting reimplementation.
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
/// path. Per-frame flow measurement consumes the commit-time parsed representation.
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
/// per-frame parsing. This re-parsing variant is invoked ONLY when the
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
/// ).
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

pub(crate) struct ShapedDocumentMetrics {
    pub signature: [u8; 32],
    pub height: f32,
    pub rows: std::sync::Arc<[(usize, f32)]>,
}

impl TextRasterizer {
    /// Create a text rasterizer whose font roles follow `font_config`.
    ///
    /// The font system never scans OS font directories (see [`crate::fonts`]):
    /// it holds the bundled faces, the configured fonts dir, and only those
    /// Windows system families the config names.  With the default config it
    /// is fully deterministic.  Agent-uploaded fonts are accepted afterwards
    /// via `load_font_bytes`.
    pub(crate) fn with_font_config(
        device: &Device,
        queue: &Queue,
        format: wgpu::TextureFormat,
        font_config: &FontConfig,
    ) -> Self {
        let font_config = font_config.clone().normalized();
        let (font_system, resolved_fonts, font_warnings) =
            build_font_system_with_diagnostics(&font_config);

        tracing::info!(
            font_face_count = font_system.db().faces().count(),
            sans = %resolved_fonts.sans,
            mono = %resolved_fonts.mono,
            serif = %resolved_fonts.serif,
            "TextRasterizer initialised (no system-font scan)"
        );

        let swash_cache = SwashCache::new();
        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let mut atlas = TextAtlas::new(device, queue, &cache, format);
        let renderer = TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);
        let overlay_renderer =
            TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);

        let menu_renderer =
            TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);

        let affordance_renderer =
            TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);

        Self {
            font_system,
            swash_cache,
            viewport,
            atlas,
            renderer,
            overlay_renderer,
            menu_renderer,
            affordance_renderer,
            loaded_font_ids: HashSet::new(),
            uploaded_fonts: Vec::new(),
            resolved_fonts,
            font_warnings,
            truncation_cache: TruncationCache::new(),
            #[cfg(any(test, feature = "dev-mode"))]
            shape_call_count: 0,
            shape_cache: HashMap::new(),
            overlay_shape_cache: HashMap::new(),
            menu_shape_cache: HashMap::new(),
            affordance_shape_cache: HashMap::new(),
            system_fallback: crate::fonts::SystemFallback::system(),
        }
    }

    /// Cumulative number of per-frame `shape_until_scroll` calls.
    /// Used by the scroll-reshape benchmark/regression to prove a pure scroll
    /// performs zero re-shapes once the shaped-buffer cache is in place.
    #[inline]
    #[cfg(any(test, feature = "dev-mode"))]
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
    pub(crate) fn load_font_bytes(&mut self, resource_id: [u8; 32], data: &[u8]) {
        if self.loaded_font_ids.contains(&resource_id) {
            tracing::debug!(
                resource_id = %format_resource_id(&resource_id),
                "font already loaded — skipping duplicate load_font_data"
            );
            return;
        }

        let bytes = Arc::new(data.to_vec());
        self.font_system
            .db_mut()
            .load_font_source(glyphon::fontdb::Source::Binary(bytes.clone()));

        self.loaded_font_ids.insert(resource_id);
        self.uploaded_fonts.push((resource_id, bytes));

        // A new face can change font fallback / substitution for text that was
        // already shaped, so every cached shaped buffer is now potentially stale
        // (the ShapeKey does not capture FontSystem state). Drop the cache; it is
        // rebuilt from the next frame's items on the miss path.
        self.shape_cache.clear();
        self.overlay_shape_cache.clear();
        self.affordance_shape_cache.clear();

        tracing::info!(
            resource_id = %format_resource_id(&resource_id),
            bytes = data.len(),
            "agent-uploaded font loaded into FontSystem"
        );
    }

    /// The concrete family each font role resolved to.
    #[inline]
    pub(crate) fn resolved_fonts(&self) -> &ResolvedFonts {
        &self.resolved_fonts
    }

    pub(crate) fn font_warnings(&self) -> &tze_hud_scene::startup_diagnostics::StartupWarnings {
        &self.font_warnings
    }

    /// Agent-uploaded fonts loaded so far, as `(resource_id, bytes)` in load
    /// order — for replaying into a rebuilt rasterizer.
    pub(crate) fn take_uploaded_fonts(&mut self) -> Vec<([u8; 32], Arc<Vec<u8>>)> {
        std::mem::take(&mut self.uploaded_fonts)
    }

    /// Returns `true` if the font identified by `resource_id` has already been
    /// loaded into the `FontSystem`.
    #[inline]
    #[cfg(test)]
    pub(crate) fn has_font(&self, resource_id: &[u8; 32]) -> bool {
        self.loaded_font_ids.contains(resource_id)
    }

    /// Total number of font faces visible to glyphon's `FontSystem`.
    #[inline]
    #[cfg(test)]
    pub(crate) fn font_face_count(&self) -> usize {
        self.font_system.db().faces().count()
    }

    /// Update the viewport resolution before each frame.
    ///
    /// Must be called once per frame before `render_text_pass`.
    pub(crate) fn update_viewport(&mut self, queue: &Queue, width: u32, height: u32) {
        self.viewport.update(queue, Resolution { width, height });
    }

    /// Measure the composer draft for horizontal caret-follow.
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
    /// (finding 1) — a one-row visual layout so the runtime's pointer
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

    /// Measure a composer draft under WORD WRAP for the multi-line profile.
    ///
    /// Shapes `text` wrapped to `wrap_width` (the composer's interior width) with
    /// the sans-serif composer font and returns `(total_lines, caret_line)`:
    /// - `total_lines` — number of wrapped visual lines (≥ 1, even for empty text).
    /// - `caret_line`  — zero-based wrapped-line index the caret at `caret_byte`
    ///   sits on (the line of the first glyph whose logical start is ≥ the caret
    ///   byte; the last line when the caret is at or past the end).
    ///
    /// `caret_byte` is snapped down to a valid UTF-8 boundary, so agent-provided
    /// offsets never panic. `text` is the RAW draft text: the caret is
    /// a chrome-layer quad now, not an inserted glyph, so the rendered text is
    /// always the raw draft and this measures the same string — the box sizing and
    /// the render never disagree, and a zero-width caret at a wrap boundary clips
    /// harmlessly (no glyph to clip).
    ///
    /// Same wrap parameters as the render path (`bounds_width = wrap_width`,
    /// [`WRAPPED_TEXT_WRAP`]), so the line counts agree — including the
    /// break-anywhere fallback that keeps an over-long unbreakable token on
    /// multiple in-box lines instead of one clipped line. Runs once
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
        // as it paints and the box height / caret line stay in sync.
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

    /// Mutable access to the rasterizer's `FontSystem` — the SAME font system the
    /// render path shapes glyphs with, so it carries any agent-uploaded fonts
    /// loaded via `load_font_data`. Exposed so the vertical-flow
    /// pre-pass (`renderer::…::prime_vertical_flow_layout`) can measure flow child
    /// heights against the real render fonts instead of a fresh bundled font
    /// system, keeping "measured == painted" true for uploaded fonts too.
    pub(crate) fn font_system_mut(&mut self) -> &mut FontSystem {
        &mut self.font_system
    }

    /// See [`single_line_width`].
    pub(crate) fn measure_single_line(
        &mut self,
        text: &str,
        font_size_px: f32,
        font_family: FontFamily,
        font_weight: u16,
    ) -> f32 {
        single_line_width(
            &mut self.font_system,
            text,
            font_size_px,
            font_family,
            font_weight,
        )
    }

    /// Measure the composer draft's wrapped VISUAL-LINE layout for soft-wrap
    /// vertical caret movement.
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
        // token.
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
            // stays single-sourced by the compositor.
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
    ///   [`overflow::truncate_tail_anchored`] (newest content
    ///   always visible for follow-tail streaming transcripts).
    /// - [`TruncationViewport::HeadAnchored`] items call
    ///   [`overflow::truncate_for_ellipsis`] (default / scrolled-back viewports).
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
    /// position data (Phase 2). The caller must render these quads
    /// in a flat-rect pass immediately before the glyphon text pass so that
    /// backdrop quads appear behind the glyphs.
    ///
    /// Returns `Ok(quads)` on success where `quads` is the list of inline
    /// backdrop quads to render, or `Err(string)` on glyphon error (non-fatal —
    /// the frame continues with missing text rather than a crash).
    pub(crate) fn prepare_text_items(
        &mut self,
        device: &Device,
        queue: &Queue,
        items: &[TextItem],
    ) -> Result<Vec<InlineBackdropQuad>, String> {
        self.prepare_layer(TextLayer::Content, device, queue, items)
    }

    /// Prepare runtime chrome text for the final overlay pass; replay
    /// it with [`TextRasterizer::render_overlay_text_pass`]. Independent of the
    /// content layer's prepared state.
    pub(crate) fn prepare_overlay_text_items(
        &mut self,
        device: &Device,
        queue: &Queue,
        items: &[TextItem],
    ) -> Result<Vec<InlineBackdropQuad>, String> {
        self.prepare_layer(TextLayer::Overlay, device, queue, items)
    }

    pub(crate) fn prepare_affordance_text_items(
        &mut self,
        device: &Device,
        queue: &Queue,
        items: &[TextItem],
    ) -> Result<Vec<InlineBackdropQuad>, String> {
        self.prepare_layer(TextLayer::Affordance, device, queue, items)
    }

    pub(crate) fn prepare_menu_text_items(
        &mut self,
        device: &Device,
        queue: &Queue,
        items: &[TextItem],
    ) -> Result<Vec<InlineBackdropQuad>, String> {
        self.prepare_layer(TextLayer::Menu, device, queue, items)
    }

    fn prepare_layer(
        &mut self,
        layer: TextLayer,
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

        // Phase 0: resolve ellipsis-truncated text
        // strings from the truncation cache.
        //
        // The cache is primed outside the frame loop by `prime_truncation_cache`
        // (called from `Compositor::prime_truncation_cache` gated on scene.version).
        // On a warm path (after at least one commit), a cache hit is one BLAKE3
        // hash to reconstruct the key plus one HashMap lookup — zero shaping work.
        //
        // The render path NEVER shapes inline. Truncation is an O(n)
        // operation and the cache exists precisely to keep it off the frame loop.
        // On a cache miss we resolve in priority order, all O(1) and shaping-free:
        //
        //   1. Exact-geometry hit (warm steady state) — the correct truncation.
        //   2. Stale-by-content hit (cadence-deferred frame): the most-recently-
        //      primed truncation for the SAME content at slightly-old geometry.
        //      Still a valid truncation (no torn glyphs, ellipsis intact), just
        //      one cadence tick behind.  The deferred off-path prime
        //      (`Compositor::prime_truncation_cache`) fills the correct entry for
        //      a subsequent frame — graceful degradation, not a bug (
        // rule: arrival ≠ presentation; never block the frame loop).
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
        // so that glyph↔run byte-offset matching is always correct.
        let mut effective_styled_runs_per_item: Vec<Vec<StyledRunItem>> =
            Vec::with_capacity(items.len());

        // Phase 1 shaped-buffer cache: move the whole cache out and
        // rebuild it from THIS frame's items. Each item's `ShapeKey` hashes every
        // input that affects the shaped glyphs (original + effective/truncated
        // text, bounds, font, weight, alignment, overflow, viewport, line-height,
        // styled/color runs) but NOT the scroll offset — a Clip pane's shaped
        // output is offset-independent, so a pure scroll hits every key and
        // re-shapes zero buffers (the whole win). A key miss (content / geometry /
        // font / runs changed) re-shapes exactly once. Buffers not reused this
        // frame, including surplus buffers for a still-live key, are dropped
        // with `old_cache`. Retain only this preparation's per-key multiplicity;
        // duplicate items each need an owned buffer through glyphon preparation.
        let mut old_cache = std::mem::take(match layer {
            TextLayer::Content => &mut self.shape_cache,
            TextLayer::Overlay => &mut self.overlay_shape_cache,
            TextLayer::Menu => &mut self.menu_shape_cache,
            TextLayer::Affordance => &mut self.affordance_shape_cache,
        });
        let mut shape_keys: Vec<[u8; 32]> = Vec::with_capacity(items.len());
        let mut buffers: Vec<Buffer> = Vec::with_capacity(items.len());

        for (item, truncated) in items.iter().zip(truncated_texts.iter()) {
            let effective_text: &str = truncated.as_deref().unwrap_or(&item.text);

            // Effective (resliced) styled runs are needed EVERY frame for inline
            // backdrop quads (their pixel positions shift with scroll), so this
            // pure run-computation runs on both the cache-hit and cache-miss
            // paths — split from the set_text/shape work that only runs on a miss.
            // THE SUBTLETY: the interleave of run-computation with
            // set_text+shape is untangled here so a hit does zero shaping while
            // pinned behaviors (styled runs, truncation reslicing, backdrop quads)
            // still reproduce byte-for-byte.
            effective_styled_runs_per_item
                .push(effective_styled_runs_for_item(item, truncated.as_deref()));

            let key = shape_key_for_item(item, effective_text);
            shape_keys.push(key);

            let buf = if let Some(reused) = old_cache.get_mut(&key).and_then(Vec::pop) {
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

        // Inline backdrop quads (Phase 2): compute pixel-exact rects
        // from glyph layout data.  Must happen here, between buffer shaping and
        // the TextArea construction below that borrows from `buffers`.
        // `compute_inline_backdrop_quads` only iterates buffers immutably.
        //
        // Pass `effective_styled_runs_per_item` (which uses the resliced styled
        // runs for truncated items) so that glyph↔byte-offset matching is correct
        // even when the buffer was shaped with different byte offsets than the
        // original `item.styled_runs` (Thread 3 fix).
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

        let renderer = match layer {
            TextLayer::Content => &mut self.renderer,
            TextLayer::Overlay => &mut self.overlay_renderer,
            TextLayer::Menu => &mut self.menu_renderer,
            TextLayer::Affordance => &mut self.affordance_renderer,
        };
        let prepare_result = renderer
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
        // current per-item buffers are retained, including duplicate keys.
        // Rebuild on both Ok and Err, preserving the ordinary prepare retry path.
        let mut rebuilt: HashMap<[u8; 32], Vec<Buffer>> = HashMap::new();
        for (key, buffer) in shape_keys.into_iter().zip(buffers) {
            rebuilt.entry(key).or_default().push(buffer);
        }
        match layer {
            TextLayer::Content => self.shape_cache = rebuilt,
            TextLayer::Overlay => self.overlay_shape_cache = rebuilt,
            TextLayer::Menu => self.menu_shape_cache = rebuilt,
            TextLayer::Affordance => self.affordance_shape_cache = rebuilt,
        }

        prepare_result.map(|()| inline_backdrop_quads)
    }

    #[cfg(any(test, feature = "dev-mode"))]
    pub(crate) fn has_shaped_document(&self, item: &TextItem) -> bool {
        self.shape_cache
            .get(&shape_key_for_item(item, &item.text))
            .is_some_and(|buffers| !buffers.is_empty())
    }

    /// Reuse the paint buffer to measure the retained document, including wrap
    /// and rich-text line metrics. The fixed pane scissor is independent.
    pub(crate) fn measure_scroll_document(&mut self, item: &TextItem) -> ShapedDocumentMetrics {
        let key = shape_key_for_item(item, &item.text);
        if !self.shape_cache.contains_key(&key) {
            let buffer = self.build_and_shape_buffer(item, None);
            self.shape_cache.insert(key, vec![buffer]);
        }
        let buffer = &self.shape_cache[&key][0];
        let mut bases = Vec::with_capacity(buffer.lines.len());
        let mut base = 0;
        for line in &buffer.lines {
            bases.push(base);
            base += line.text().len() + 1;
        }
        let mut height = 0.0_f32;
        let rows = buffer
            .layout_runs()
            .map(|run| {
                height = height.max(run.line_top + run.line_height);
                let start = run
                    .glyphs
                    .iter()
                    .map(|glyph| glyph.start)
                    .min()
                    .unwrap_or(0);
                (bases[run.line_i] + start, run.line_top)
            })
            .collect::<Vec<_>>();
        ShapedDocumentMetrics {
            signature: key,
            height,
            rows: rows.into(),
        }
    }

    /// Glyph extents of the shaped document, in row order, x relative to the
    /// clip (viewport) left edge. Call after [`Self::measure_scroll_document`].
    pub(crate) fn shaped_glyphs(
        &self,
        item: &TextItem,
    ) -> (Vec<tze_hud_scene::PortalGlyph>, Vec<u32>) {
        let mut glyphs = Vec::new();
        let mut row_glyphs = Vec::new();
        let Some(buffer) = self
            .shape_cache
            .get(&shape_key_for_item(item, &item.text))
            .and_then(|b| b.first())
        else {
            return (glyphs, row_glyphs);
        };
        let mut bases = Vec::with_capacity(buffer.lines.len());
        let mut base = 0;
        for line in &buffer.lines {
            bases.push(base);
            base += line.text().len() + 1;
        }
        let dx = item.pixel_x - item.clip_pixel_x;
        for run in buffer.layout_runs() {
            row_glyphs.push(glyphs.len() as u32);
            let base = bases[run.line_i];
            glyphs.extend(run.glyphs.iter().map(|g| tze_hud_scene::PortalGlyph {
                start: (base + g.start) as u32,
                end: (base + g.end) as u32,
                x: g.x + dx,
                w: g.w,
            }));
        }
        (glyphs, row_glyphs)
    }

    /// Build and shape on a cache miss; scroll frames reuse this exact buffer.
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
        // overflowing into the clip scissor. Shared with the composer
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
            // Phase-1 markdown styled-run path.
            //
            // `styled_runs` carry per-span weight, italic, monospace, and
            // optional color from the parse cache.  We build `(text_slice,
            // Attrs)` pairs and call `set_rich_text` once — zero re-parse cost.
            //
            // When ellipsis truncation was applied, byte offsets in `styled_runs`
            // must be remapped to the truncated string's coordinate system
            //. HeadAnchored: "{prefix}…" (clamp at prefix_len).
            // TailAnchored: "…\n{tail}" (leading ellipsis, — offsets
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
            // to the truncated string's coordinate system.
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
        // Tests and dev-mode work observation count every per-frame shape to prove a pure scroll (unchanged
        // shape inputs, cache hit) performs zero re-shapes.
        #[cfg(any(test, feature = "dev-mode"))]
        {
            self.shape_call_count += 1;
        }
        buf.shape_until_scroll(&mut self.font_system, false);
        self.note_glyph_misses(&buf);
        buf
    }

    /// Hand every glyph no loaded face covered (`.notdef`, glyph id 0) to the
    /// lazy system fallback, which loads the families cosmic-text names for it
    /// off the frame thread (hud-ljhku.13). Runs only when a buffer is
    /// actually shaped (shaped-buffer cache miss), never per frame.
    fn note_glyph_misses(&mut self, buf: &Buffer) {
        let mut missing = String::new();
        for run in buf.layout_runs() {
            for glyph in run.glyphs.iter().filter(|g| g.glyph_id == 0) {
                if let Some(text) = run.text.get(glyph.start..glyph.end) {
                    missing.push_str(text);
                }
            }
        }
        if !missing.is_empty() {
            let locale = self.font_system.locale().to_owned();
            self.system_fallback.note_missing(&missing, &locale);
        }
    }

    /// Install system fallback faces whose background load landed
    /// (hud-ljhku.13). When any did, every shaped buffer and truncation
    /// point is stale (they were shaped with tofu): both caches are dropped
    /// so each text area re-lays out once on the next prepare. Returns
    /// whether faces were added.
    pub(crate) fn poll_system_fallback(&mut self) -> bool {
        if !self.system_fallback.poll(&mut self.font_system) {
            return false;
        }
        self.shape_cache.clear();
        self.overlay_shape_cache.clear();
        self.menu_shape_cache.clear();
        self.affordance_shape_cache.clear();
        self.truncation_cache.clear();
        true
    }

    /// The lazy system fallback loader, for wiring and tests.
    pub(crate) fn system_fallback_mut(&mut self) -> &mut crate::fonts::SystemFallback {
        &mut self.system_fallback
    }

    /// Take the fallback loader (with its installed faces) for a rebuilt
    /// rasterizer; see [`Self::adopt_system_fallback`].
    pub(crate) fn take_system_fallback(&mut self) -> crate::fonts::SystemFallback {
        std::mem::replace(
            &mut self.system_fallback,
            crate::fonts::SystemFallback::system(),
        )
    }

    /// Adopt a previous rasterizer's fallback loader, replaying its installed
    /// faces into this font system so no family loads twice.
    pub(crate) fn adopt_system_fallback(&mut self, fallback: crate::fonts::SystemFallback) {
        fallback.replay_into(&mut self.font_system);
        self.system_fallback = fallback;
    }

    /// Record the glyphon text pass into `render_pass`.
    ///
    /// The render pass must have been begun with `LoadOp::Load` so that prior
    /// geometry (rects, backgrounds) is preserved under the text.
    pub(crate) fn render_text_pass<'rp>(
        &'rp self,
        render_pass: &mut wgpu::RenderPass<'rp>,
    ) -> Result<(), String> {
        self.renderer
            .render(&self.atlas, &self.viewport, render_pass)
            .map_err(|e| format!("glyphon render: {e:?}"))
    }

    /// Record the overlay-layer text pass (see [`TextRasterizer::prepare_overlay_text_items`]).
    /// Same `LoadOp::Load` contract as [`TextRasterizer::render_text_pass`].
    pub(crate) fn render_overlay_text_pass<'rp>(
        &'rp self,
        render_pass: &mut wgpu::RenderPass<'rp>,
    ) -> Result<(), String> {
        self.overlay_renderer
            .render(&self.atlas, &self.viewport, render_pass)
            .map_err(|e| format!("glyphon overlay render: {e:?}"))
    }

    pub(crate) fn render_affordance_text_pass<'rp>(
        &'rp self,
        render_pass: &mut wgpu::RenderPass<'rp>,
    ) -> Result<(), String> {
        self.affordance_renderer
            .render(&self.atlas, &self.viewport, render_pass)
            .map_err(|e| format!("glyphon affordance render: {e:?}"))
    }

    pub(crate) fn render_menu_text_pass<'rp>(
        &'rp self,
        render_pass: &mut wgpu::RenderPass<'rp>,
    ) -> Result<(), String> {
        self.menu_renderer
            .render(&self.atlas, &self.viewport, render_pass)
            .map_err(|e| format!("glyphon menu render: {e:?}"))
    }

    /// Trim the atlas after the frame is presented.
    ///
    /// Reclaims memory for glyphs that were not used in the last frame.
    pub(crate) fn trim_atlas(&mut self) {
        self.atlas.trim();
    }
}

// ─── TextItem ─────────────────────────────────────────────────────────────────

/// A pixel-exact backdrop quad for an inline code span (Phase 2).
///
/// Positioned from actual glyph layout data (start/end advance, line_top/line_height
/// from cosmic-text's `LayoutRun`) after `shape_until_scroll`.  The color comes
/// from `StyledRunItem::background_color` (the `color.code.background` design token)
/// — never hardcoded.
///
/// Coordinates are in physical pixels, absolute (not tile-relative).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InlineBackdropQuad {
    /// Left edge in physical pixels.
    pub(crate) x: f32,
    /// Top edge in physical pixels.
    pub(crate) y: f32,
    /// Width in physical pixels.
    pub(crate) w: f32,
    /// Height in physical pixels.
    pub(crate) h: f32,
    /// sRGB backdrop color [r, g, b, a] from the design token.
    pub(crate) color: [u8; 4],
}

/// Compute the shaped-buffer cache key for `item`.
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

/// Effective (resliced) styled runs used for a frame's inline backdrop quads.
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
/// multi-line text-selection shape.
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

pub(crate) fn compute_inline_backdrop_quads(
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
        // full-text byte range as `line_offset + glyph.start.. line_offset + glyph.end`.
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

            // Full-text byte span of THIS visual line: the min/max glyph
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
                    // Full-width selection shape: when this selection run
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
    /// Markdown-derived styled runs from the Phase-1 parse cache.
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
    ///   `max_lines` (static tiles, scrolled-back viewports).
    /// - [`TruncationViewport::TailAnchored`] — show the LAST `max_lines`
    ///   (follow-tail streaming tiles at the tail).
    ///
    /// Set by `collect_text_items_from_node` / `collect_ellipsis_text_items_from_node`
    /// based on the tile's current `follow_tail_at_tail` state in the scene.
    pub viewport: TruncationViewport,
    /// Line-height multiplier: `line_height_px = font_size_px × line_height_multiplier`.
    ///
    /// Resolved from the `typography.line_height.multiplier` design token via
    /// [`ParsedMarkdown::line_height_multiplier`].  Default `1.4`.
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
    /// text-selection shape rather than tight per-glyph quads: the
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
/// whether the lossy raw-content path is required.
pub(crate) fn markdown_node_has_pixel_runs(node: &TextMarkdownNode) -> bool {
    node.color_runs.iter().any(|r| r.start_byte < r.end_byte)
}

/// Token-resolved accent for an active transcript-tail streaming cursor, if this
/// node carries the streaming-cursor tail marker.
///
/// The projection adapter (`streaming_cursor_color_runs` in
/// `tze_hud_portal::render`) signals an active tail cursor with a
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
pub(crate) type ClipBox = (f32, f32, f32, f32);

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
/// composer strip) — the render-side meaning of "one scene node per part".
/// The part region is a *clip* envelope only; it deliberately does
/// **not** alter the wrap/truncation measure (`bounds_width`), so the
/// shaped-buffer and truncation caches stay keyed exactly as before (PR #1007
/// perf invariant).
///
/// Each returned dimension is floored at `1.0` so a fully-collapsed
/// intersection still yields a valid (degenerate) scissor rect rather than a
/// zero or negative extent.
pub(crate) fn portal_part_clip_rect(
    content: ClipBox,
    tile: ClipBox,
    part: Option<ClipBox>,
) -> ClipBox {
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
    /// fallback parses inline via `parse_markdown_subset` rather
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
    /// [`ParsedMarkdown`] from the [`MarkdownCache`] (Phase-1 ).
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
    pub(crate) fn from_text_markdown_cached(
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
        // path. Assert in debug builds so genuine misuse — passing
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
        // vertical-flow measurement seam via
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
    /// `scale` (px per dp) sizes the 8 dp margin and 16 dp font fallbacks.
    ///
    /// [`RenderingPolicy`]: tze_hud_scene::types::RenderingPolicy
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_zone_policy(
        text: &str,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        policy: &RenderingPolicy,
        opacity: f32,
        scale: f32,
    ) -> Self {
        // Margin: prefer margin_horizontal/margin_vertical; fall back to margin_px;
        // then fall back to 8 dp. margin_horizontal
        // "overrides margin_px for the horizontal axis. When None, falls back to margin_px."
        // Policy fields are already pixels; only the dp fallbacks need `scale`.
        let margin_h = policy
            .margin_horizontal
            .or(policy.margin_px)
            .unwrap_or(8.0 * scale);
        let margin_v = policy
            .margin_vertical
            .or(policy.margin_px)
            .unwrap_or(8.0 * scale);

        let font_size_px = policy
            .font_size_px
            .unwrap_or(16.0 * scale)
            .clamp(6.0, 200.0);
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
pub(crate) fn rgba_to_srgb_u8(c: Rgba) -> [u8; 4] {
    [
        linear_to_srgb_u8(c.r),
        linear_to_srgb_u8(c.g),
        linear_to_srgb_u8(c.b),
        (c.a * 255.0).clamp(0.0, 255.0) as u8,
    ]
}

/// Multiply the alpha channel of an sRGB u8 color by `opacity`.
pub(crate) fn apply_opacity_to_color(color: [u8; 4], opacity: f32) -> [u8; 4] {
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
/// filter used by [`TextItem::from_text_markdown_cached`].
///
/// Shared by [`TextItem::from_text_markdown_cached`] (the render path) and
/// [`measure_markdown_content_height`] (the vertical-flow measurement seam)
/// so both build the identical run set from the identical parse —
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
/// instead emits a single inline-leading-ellipsis line
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
/// `max_lines == 1` case). The original `styled_runs` byte offsets are
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
/// Used when priming the truncation cache for items with styled runs.
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
    // Use effective measurement attrs from styled runs.
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
pub(crate) fn strip_markdown_v1(s: &str) -> String {
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
    use std::ops::Range;
    use tze_hud_scene::types::{Rect, RenderingPolicy, Rgba, TextMarkdownNode};

    fn item(text: &str, width: f32, height: f32) -> TextItem {
        TextItem {
            text: Arc::from(text),
            pixel_x: 10.0,
            pixel_y: 20.0,
            bounds_width: width,
            bounds_height: height,
            clip_pixel_x: 10.0,
            clip_pixel_y: 20.0,
            clip_bounds_width: width,
            clip_bounds_height: height,
            font_size_px: 16.0,
            line_height_multiplier: 1.4,
            font_family: FontFamily::SystemSansSerif,
            font_weight: 400,
            color: [255; 4],
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            outline_color: None,
            outline_width: None,
            opacity: 1.0,
            color_runs: Box::new([]),
            styled_runs: Box::new([]),
            viewport: TruncationViewport::HeadAnchored,
        }
    }

    fn styled(bytes: Range<usize>) -> StyledRunItem {
        StyledRunItem {
            start_byte: bytes.start,
            end_byte: bytes.end,
            weight: Some(700),
            italic: true,
            monospace: false,
            color: None,
            background_color: Some([30, 30, 30, 255]),
            size_scale: Some(1.2),
            fill_line_width: false,
        }
    }

    fn color(bytes: Range<usize>) -> ColorRunItem {
        ColorRunItem {
            start_byte: bytes.start,
            end_byte: bytes.end,
            color: [10, 20, 30, 255],
        }
    }

    fn prime<'a>(
        cache: &'a mut TruncationCache,
        item: &TextItem,
        fonts: &mut FontSystem,
    ) -> &'a TruncationResult {
        let key = effective_truncation_key(item).unwrap();
        let (_, monospace) = styled_runs_effective_measurement(&item.styled_runs, item.font_weight);
        let family = if monospace {
            FontFamily::SystemMonospace
        } else {
            item.font_family
        };
        cache.prime(
            key,
            &item.text,
            item.bounds_width,
            item.bounds_height,
            item.font_size_px,
            family,
            key.font_weight,
            item.viewport,
            item.line_height_multiplier,
            fonts,
        )
    }

    fn shape(item: &TextItem, text: &str, fonts: &mut FontSystem) -> Buffer {
        let mut buffer = Buffer::new(
            fonts,
            Metrics::new(
                item.font_size_px,
                item.font_size_px * item.line_height_multiplier,
            ),
        );
        buffer.set_size(fonts, Some(item.bounds_width), None);
        buffer.set_wrap(fonts, Wrap::Word);
        buffer.set_text(
            fonts,
            text,
            Attrs::new()
                .family(Family::SansSerif)
                .weight(Weight(item.font_weight)),
            Shaping::Advanced,
        );
        buffer.shape_until_scroll(fonts, false);
        buffer
    }

    #[test]
    fn portal_part_clip_rect_intersects_content_tile_and_absolute_part() {
        for (content, tile, part, expected) in [
            (
                (10.0, 10.0, 400.0, 400.0),
                (0.0, 0.0, 100.0, 100.0),
                None,
                (10.0, 10.0, 90.0, 90.0),
            ),
            (
                (0.0, 0.0, 100.0, 100.0),
                (0.0, 0.0, 80.0, 80.0),
                Some((0.0, 0.0, 200.0, 200.0)),
                (0.0, 0.0, 80.0, 80.0),
            ),
            (
                (0.0, 0.0, 100.0, 100.0),
                (0.0, 0.0, 100.0, 100.0),
                Some((0.0, 0.0, 100.0, 60.0)),
                (0.0, 0.0, 100.0, 60.0),
            ),
            (
                (200.0, 100.0, 300.0, 400.0),
                (200.0, 100.0, 300.0, 400.0),
                Some((200.0, 100.0, 300.0, 50.0)),
                (200.0, 100.0, 300.0, 50.0),
            ),
            (
                (0.0, 0.0, 40.0, 40.0),
                (0.0, 0.0, 100.0, 100.0),
                Some((60.0, 60.0, 40.0, 40.0)),
                (60.0, 60.0, 1.0, 1.0),
            ),
        ] {
            assert_eq!(portal_part_clip_rect(content, tile, part), expected);
        }
    }

    #[test]
    fn composer_wraps_words_and_overwide_tokens_inside_the_box() {
        let mut fonts = crate::fonts::bundled_font_system();
        for (text, single_line) in [
            ("hi".to_owned(), true),
            ("word ".repeat(40), false),
            ("a".repeat(200), false),
        ] {
            let widths = composer_wrap_line_widths(&mut fonts, &text, 80.0, 16.0, 1.4);
            assert!(!widths.is_empty());
            assert_eq!(widths.len() == 1, single_line);
            assert!(widths.iter().all(|&width| width <= 81.0), "{widths:?}");
        }
    }

    #[test]
    fn legacy_markdown_and_linear_color_conversions() {
        for (markdown, plain) in [
            ("# Hello\n## World\nPlain text", "Hello\nWorld\nPlain text"),
            ("Hello *world*", "Hello world"),
            ("", ""),
        ] {
            assert_eq!(strip_markdown_v1(markdown), plain);
        }
        for (linear, srgb) in [(0.0, 0), (0.5, 188), (1.0, 255)] {
            assert_eq!(linear_to_srgb_u8(linear), srgb);
        }
        assert_eq!(rgba_to_srgb_u8(Rgba::BLACK), [0, 0, 0, 255]);
        assert_eq!(rgba_to_srgb_u8(Rgba::WHITE), [255; 4]);
        assert_eq!(
            apply_opacity_to_color([200, 100, 50, 200], 0.5),
            [200, 100, 50, 100]
        );
    }

    #[test]
    fn text_item_insets_markdown_without_collapsing_small_labels() {
        let mut node = TextMarkdownNode {
            content: "# Hello\n*world*".to_owned(),
            bounds: Rect::new(10.0, 20.0, 200.0, 80.0),
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        };
        let normal = TextItem::from_text_markdown_node(&node, 50.0, 50.0);
        assert_eq!((normal.pixel_x, normal.pixel_y), (66.0, 76.0));
        assert_eq!(&*normal.text, "Hello\nworld");
        assert_eq!((normal.outline_color, normal.outline_width), (None, None));
        assert_eq!((normal.font_weight, normal.opacity), (400, 1.0));

        node.bounds = Rect::new(96.0, 18.0, 152.0, 12.0);
        node.font_size_px = 11.0;
        let compact = TextItem::from_text_markdown_node(&node, 0.0, 0.0);
        assert!(compact.bounds_height >= 11.0 && compact.pixel_y < 22.0);
    }

    #[test]
    fn from_zone_policy_maps_policy_fields() {
        let build = |policy: &RenderingPolicy, opacity| {
            TextItem::from_zone_policy("text", 100.0, 200.0, 400.0, 100.0, policy, opacity, 1.0)
        };
        let defaults = build(&RenderingPolicy::default(), 1.0);
        assert_eq!((defaults.pixel_x, defaults.pixel_y), (108.0, 208.0));
        assert_eq!(defaults.font_weight, 400);
        assert_eq!(defaults.overflow, TextOverflow::Clip);

        let mut policy = RenderingPolicy {
            font_size_px: Some(24.0),
            font_weight: Some(700),
            text_color: Some(Rgba::new(1.0, 0.0, 0.0, 1.0)),
            outline_color: Some(Rgba::BLACK),
            outline_width: Some(2.0),
            margin_px: Some(20.0),
            overflow: Some(TextOverflow::Ellipsis),
            ..Default::default()
        };
        let custom = build(&policy, 0.5);
        assert_eq!((custom.pixel_x, custom.pixel_y), (120.0, 220.0));
        assert_eq!((custom.font_size_px, custom.font_weight), (24.0, 700));
        assert_eq!(custom.color, [255, 0, 0, 127]);
        assert_eq!(
            (custom.outline_color, custom.outline_width),
            (Some([0, 0, 0, 127]), Some(2.0))
        );
        assert_eq!(custom.overflow, TextOverflow::Ellipsis);

        policy.outline_width = Some(0.0);
        policy.overflow = Some(TextOverflow::Clip);
        let disabled = build(&policy, 1.0);
        assert_eq!(
            (disabled.outline_color, disabled.outline_width),
            (None, None)
        );
        assert_eq!(disabled.overflow, TextOverflow::Clip);
    }

    #[test]
    fn color_run_spans_cases() {
        let red = [255, 0, 0, 255];
        let green = [0, 255, 0, 255];
        let blue = [0, 0, 255, 255];
        let amber = [255, 128, 0, 160];
        let (r, g, b, a) = (Some(red), Some(green), Some(blue), Some(amber));
        type Run = (usize, usize, [u8; 4]);
        type Painted<'a> = (&'a str, Option<[u8; 4]>);

        // Input order settles overlaps, including runs whose start is earlier.
        // Missing or empty runs use base attributes. Newlines are normal bytes.
        let cases: &[(&str, &[Run], &[Painted<'_>])] = &[
            (
                "hello world",
                &[(0, 5, red), (6, 11, blue)],
                &[("hello", r), (" ", None), ("world", b)],
            ),
            (
                "hello world",
                &[(0, 5, red)],
                &[("hello", r), (" world", None)],
            ),
            ("no color", &[], &[("no color", None)]),
            ("text", &[(2, 2, red)], &[("text", None)]),
            ("éworld", &[(0, 2, red)], &[("é", r), ("world", None)]),
            (
                "0123456789abcde",
                &[(0, 8, red), (4, 12, green), (10, 15, blue)],
                &[("0123", r), ("456789", g), ("abcde", b)],
            ),
            (
                "abcdef",
                &[(0, 3, red), (3, 6, blue)],
                &[("abc", r), ("def", b)],
            ),
            (
                "0123456789ab",
                &[(0, 12, red), (4, 8, blue)],
                &[("0123", r), ("4567", b), ("89ab", r)],
            ),
            (
                "0123456789abcde",
                &[(5, 15, blue), (0, 10, red)],
                &[("0123456789", r), ("abcde", b)],
            ),
            (
                "hello",
                &[(2, 2, red), (3, 100, red), (0, 3, red)],
                &[("hello", r)],
            ),
            (
                "line 1\nline 2\nline 3",
                &[(3, 16, amber)],
                &[("lin", None), ("e 1\nline 2\nli", a), ("ne 3", None)],
            ),
            (
                "red line\nblue line\ngreen line",
                &[(0, 8, red), (9, 18, blue), (19, 29, green)],
                &[
                    ("red line", r),
                    ("\n", None),
                    ("blue line", b),
                    ("\n", None),
                    ("green line", g),
                ],
            ),
        ];

        for (text, runs, expected) in cases {
            let runs: Vec<_> = runs
                .iter()
                .map(|&(start_byte, end_byte, color)| ColorRunItem {
                    start_byte,
                    end_byte,
                    color,
                })
                .collect();
            let spans = color_run_spans(text, &runs, Attrs::new());
            let painted: Vec<_> = spans
                .iter()
                .map(|(text, attrs)| (*text, attrs.color_opt.map(|c| [c.r(), c.g(), c.b(), c.a()])))
                .collect();
            assert_eq!(painted, *expected, "{text:?}: {runs:?}");
        }
    }

    #[test]
    fn truncation_cache_tracks_intermediate_geometries_and_evicts_stale_content() {
        let mut fonts = crate::fonts::bundled_font_system();
        let mut cache = TruncationCache::new();
        let content = "The most recent transcript entry has a useful tail to preserve.";
        let large = item(content, 400.0, 110.0);
        let large_key = effective_truncation_key(&large).unwrap();
        assert!(cache.get_by_key(&large_key).is_none());
        assert!(
            cache
                .get_stale_by_content(&large_key.content_hash)
                .is_none()
        );

        let whole = prime(&mut cache, &large, &mut fonts).clone();
        assert_eq!(whole.text, content);
        assert!(!whole.was_truncated);
        let mut results = Vec::new();
        for (width, height) in [(85.0, 40.0), (110.0, 40.0), (110.0, 63.0), (140.0, 85.0)] {
            let resized = item(content, width, height);
            let key = effective_truncation_key(&resized).unwrap();
            assert!(
                cache.get_by_key(&key).is_none(),
                "duplicate key at {width}×{height}"
            );
            assert_eq!(
                cache.get_stale_by_content(&key.content_hash),
                Some(results.last().map_or(&whole, |(_, result)| result)),
            );
            let result = prime(&mut cache, &resized, &mut fonts).clone();
            assert!(result.was_truncated && result.text.ends_with(overflow::ELLIPSIS));
            assert_eq!(cache.get_by_key(&key), Some(&result));
            // Reuse the cached result for unchanged content, as with unchanged Markdown cache keys.
            for _ in 0..3 {
                assert_eq!(prime(&mut cache, &resized, &mut fonts), &result);
            }
            results.push((key, result));
        }
        assert_eq!(cache.len(), results.len() + 1);
        assert_ne!(results[0].1, results[2].1);
        for (key, result) in &results {
            assert_eq!(cache.get_by_key(key), Some(result));
        }

        let other = item("This content left the scene entirely", 400.0, 110.0);
        let other_key = effective_truncation_key(&other).unwrap();
        let other_result = prime(&mut cache, &other, &mut fonts).clone();
        assert_eq!(
            cache.get_stale_by_content(&other_key.content_hash),
            Some(&other_result)
        );

        let (latest_key, latest) = results.last().unwrap();
        cache.evict_except(&[*latest_key]);
        assert_eq!(cache.len(), 1);
        assert!(cache.get_by_key(&large_key).is_none());
        assert!(cache.get_by_key(&other_key).is_none());
        assert!(
            cache
                .get_stale_by_content(&other_key.content_hash)
                .is_none()
        );
        assert!(!cache.by_content.contains_key(&other_key.content_hash));
        assert_eq!(
            cache.get_stale_by_content(&large_key.content_hash),
            Some(latest)
        );
        cache.evict_except(&[]);
        assert!(
            cache
                .get_stale_by_content(&large_key.content_hash)
                .is_none()
        );
        assert!(cache.is_empty());
        assert_eq!(prime(&mut cache, &large, &mut fonts), &whole);
        cache.clear();
        assert!(cache.get_by_key(&large_key).is_none());
        assert!(
            cache
                .get_stale_by_content(&large_key.content_hash)
                .is_none()
        );
        assert!(cache.is_empty());
    }

    #[test]
    fn configured_truncation_bound_uses_the_visible_input_window() {
        let mut fonts = crate::fonts::bundled_font_system();
        let text = (0..40)
            .map(|row| {
                format!("Transcript line {row}: the quick brown fox jumps over the lazy dog.")
            })
            .collect::<Vec<_>>()
            .join("\n");
        for viewport in [
            TruncationViewport::HeadAnchored,
            TruncationViewport::TailAnchored,
        ] {
            let mut full = item(&text, 600.0, 80.0);
            full.viewport = viewport;
            let mut cache = TruncationCache::new();
            let default = cache.max_truncation_input_bytes();
            assert_eq!(default, overflow::DEFAULT_MAX_TRUNCATION_INPUT_BYTES);
            assert!((1024..default).contains(&text.len()));
            let window = |bound| {
                overflow::viewport_adjacent_input(
                    &text,
                    full.bounds_height,
                    full.font_size_px * full.line_height_multiplier,
                    viewport,
                    bound,
                    overflow::TRUNCATION_OVERSCAN_LINES,
                )
            };
            assert_eq!(window(default), text);
            assert!(window(1024).len() < text.len());
            let direct = TextItem {
                text: Arc::from(window(1024)),
                ..full.clone()
            };
            cache.set_max_truncation_input_bytes(1024);
            assert_eq!(cache.max_truncation_input_bytes(), 1024);
            let bounded = prime(&mut cache, &full, &mut fonts).clone();
            let mut unbounded = TruncationCache::new();
            unbounded.set_max_truncation_input_bytes(usize::MAX);
            assert_eq!(
                bounded.text,
                prime(&mut unbounded, &direct, &mut fonts).text
            );
        }
    }

    #[test]
    fn clipping_reslices_styled_and_colored_runs_at_visible_byte_boundaries() {
        let mut head = item("first then more", 80.0, 40.0);
        head.styled_runs = [styled(0..2), styled(3..9), styled(9..14)].into();
        let visible = reslice_styled_runs_for_item(&head, "first…");
        assert_eq!(visible.len(), 2);
        assert_eq!((visible[0].start_byte, visible[0].end_byte), (0, 2));
        assert_eq!((visible[1].start_byte, visible[1].end_byte), (3, 5));
        assert_eq!(visible[1].weight, head.styled_runs[1].weight);
        assert_eq!(
            visible[1].background_color,
            head.styled_runs[1].background_color
        );

        let mut bold = item("Hello world", 80.0, 40.0);
        bold.styled_runs = [styled(0..8)].into();
        let clipped = "Hello…";
        let visible = reslice_styled_runs_for_item(&bold, clipped);
        let spans = styled_run_spans(
            clipped,
            &visible,
            Attrs::new().family(Family::SansSerif).weight(Weight(400)),
            bold.font_family,
            bold.font_size_px,
            bold.line_height_multiplier,
        );
        let weights: Vec<_> = spans
            .iter()
            .map(|(text, attrs)| (*text, attrs.weight))
            .collect();
        assert_eq!(weights, [("Hello", Weight(700)), ("…", Weight(400))]);

        let color_runs = [color(0..2), color(3..9), color(9..14)];
        let colors = reslice_color_runs(&color_runs, 5);
        assert_eq!(colors.len(), 2);
        assert_eq!((colors[0].start_byte, colors[0].end_byte), (0, 2));
        assert_eq!(
            (colors[1].start_byte, colors[1].end_byte, colors[1].color),
            (3, 5, color_runs[1].color)
        );

        let mut tail = item("one\nélan\nlast", 80.0, 40.0);
        tail.viewport = TruncationViewport::TailAnchored;
        tail.styled_runs = [styled(0..3), styled(1..9), styled(10..14)].into();
        let color_runs = [
            ColorRunItem {
                color: [255, 128, 0, 255],
                ..color(0..3)
            },
            ColorRunItem {
                color: [0, 128, 255, 255],
                ..color(1..9)
            },
            color(10..14),
        ];
        for (text, spans) in [
            (
                "…\nélan\nlast",
                &[
                    (4, 9, "élan", [0, 128, 255, 255]),
                    (10, 14, "last", [10, 20, 30, 255]),
                ][..],
            ),
            ("…last", &[(3, 7, "last", [10, 20, 30, 255])][..]),
            ("…\nél…\nlast", &[][..]),
        ] {
            let styles = effective_styled_runs_for_item(&tail, Some(text));
            let colors = reslice_color_runs_tail_anchored(&tail.text, text, &color_runs);
            assert_eq!(styles.len(), spans.len(), "{text}");
            assert_eq!(colors.len(), spans.len(), "{text}");
            for ((style, color), &(start, end, word, rgba)) in styles.iter().zip(&colors).zip(spans)
            {
                assert_eq!((style.start_byte, style.end_byte), (start, end));
                assert_eq!(
                    (color.start_byte, color.end_byte, color.color),
                    (start, end, rgba)
                );
                assert_eq!(&text[start..end], word);
                assert_eq!(style.weight, Some(700));
                assert_eq!(style.size_scale, Some(1.2));
                assert!(style.italic);
            }
        }

        tail.text = Arc::from("…already_short");
        tail.styled_runs = [styled(0..3), styled(3..14)].into();
        let unchanged = effective_styled_runs_for_item(&tail, Some(&tail.text));
        assert_eq!(unchanged.len(), 2);
        assert_eq!((unchanged[0].start_byte, unchanged[0].end_byte), (0, 3));
        assert_eq!((unchanged[1].start_byte, unchanged[1].end_byte), (3, 14));
    }

    #[test]
    fn truncation_measures_effective_weight_and_monospace_and_keeps_their_keys_separate() {
        let mut fonts = crate::fonts::bundled_font_system();
        let mut cache = TruncationCache::new();
        for text in [
            "WWW bold WWW bold WWW bold WWW bold",
            "WWW ffi e\u{301} 👨‍👩‍👧‍👦 WWW tail",
            "العربية الطويلة WWW العربية ffi",
            "heading\nWWW ffi e\u{301} 👨‍👩‍👧‍👦\nlast row",
        ] {
            let mut measured = item(text, 120.0, 40.0);
            let regular_key = effective_truncation_key(&measured).unwrap();
            let regular = prime(&mut cache, &measured, &mut fonts).clone();
            measured.styled_runs = [
                StyledRunItem {
                    weight: Some(600),
                    ..styled(0..text.len())
                },
                styled(0..text.len()),
                StyledRunItem {
                    weight: Some(500),
                    ..styled(0..text.len())
                },
            ]
            .into();
            let bold_key = effective_truncation_key(&measured).unwrap();
            assert_ne!(regular_key, bold_key);
            assert_eq!(bold_key.font_weight, 700);
            let bold = prime(&mut cache, &measured, &mut fonts).clone();
            assert_eq!(cache.get_by_key(&regular_key), Some(&regular));
            assert_eq!(cache.get_by_key(&bold_key), Some(&bold));
            let entries = cache.len();
            assert_eq!(prime(&mut cache, &measured, &mut fonts), &bold);
            assert_eq!(cache.len(), entries);
            if text.starts_with("WWW bold") {
                assert!(regular.was_truncated && bold.was_truncated);
                assert!(bold.text.len() <= regular.text.len());
            }
            for (weight, result) in [(400, regular), (700, bold)] {
                measured.font_weight = weight;
                let reference = shape(&measured, &result.text, &mut fonts);
                let runs: Vec<_> = reference.layout_runs().collect();
                assert_eq!(runs.len(), 1, "{text:?} at weight {weight}");
                assert!(runs[0].line_w <= measured.bounds_width);
            }
            measured.font_weight = 400;
            measured.styled_runs[0].monospace = true;
            let mono_key = effective_truncation_key(&measured).unwrap();
            assert_eq!(
                mono_key,
                TruncationKey::new(
                    text,
                    120.0,
                    40.0,
                    16.0,
                    FontFamily::SystemMonospace,
                    700,
                    TruncationViewport::HeadAnchored,
                )
            );
            assert_ne!(mono_key, bold_key);
            measured.overflow = TextOverflow::Clip;
            assert_eq!(effective_truncation_key(&measured), None);
        }

        let monospace = StyledRunItem {
            weight: None,
            monospace: true,
            ..styled(0..4)
        };
        assert_eq!(
            styled_runs_effective_measurement(&[monospace], 400),
            (400, true)
        );

        for text in [
            "",
            "العربية ffi",
            "e\u{301} 👨‍👩‍👧‍👦",
            "LTR e\u{301} 👨‍👩‍👧‍👦",
            "one\ttwo",
            "first\r\nsecond\nthird",
        ] {
            let mut fitted = item(text, 400.0, 100.0);
            for weight in [700, 400, 700] {
                fitted.font_weight = weight;
                let reference = shape(&fitted, text, &mut fonts);
                assert!(reference.layout_runs().count() <= 4);
                for run in reference.layout_runs() {
                    assert!(
                        run.line_w <= fitted.bounds_width,
                        "{text:?} at weight {weight}"
                    );
                    let start = run
                        .glyphs
                        .iter()
                        .map(|glyph| glyph.start)
                        .min()
                        .unwrap_or(0);
                    assert!(reference.lines[run.line_i].text().is_char_boundary(start));
                }

                let key = effective_truncation_key(&fitted).unwrap();
                let result = prime(&mut cache, &fitted, &mut fonts).clone();
                assert_eq!(result.text, text, "{text:?} at weight {weight}");
                assert!(!result.was_truncated);
                let entries = cache.len();
                assert_eq!(prime(&mut cache, &fitted, &mut fonts), &result);
                assert_eq!(cache.len(), entries);
                assert_eq!(cache.get_by_key(&key), Some(&result));
            }
        }
    }

    #[test]
    fn glyph_layout_drives_inline_backdrops_and_multiline_selection() {
        let mut fonts = crate::fonts::bundled_font_system();
        let quads = |item: &TextItem, fonts: &mut FontSystem| {
            let buffer = shape(item, &item.text, fonts);
            compute_inline_backdrop_quads(
                std::slice::from_ref(item),
                &[buffer],
                &[item.styled_runs.to_vec()],
            )
        };

        let mut code = item("hello code world", 400.0, 60.0);
        let background = [30, 30, 30, 255];
        code.styled_runs = [styled(6..10)].into();
        let code_quads = quads(&code, &mut fonts);
        assert_eq!(code_quads.len(), 1);
        let quad = &code_quads[0];
        assert_eq!(quad.color, background);
        assert!(quad.x > code.pixel_x && quad.y >= code.pixel_y && quad.w > 0.0 && quad.h > 0.0);
        code.styled_runs[0].fill_line_width = true;
        let single_line_selection = quads(&code, &mut fonts);
        assert_eq!(single_line_selection.len(), 1);
        let filled = &single_line_selection[0];
        assert_eq!((filled.x, filled.w), (quad.x, quad.w));

        for (text, width, bytes) in [("aaaa bbbb cccc", 45.0, 1..13), ("aa\nbb\ncc", 300.0, 1..7)] {
            let mut selection = item(text, width, 200.0);
            selection.styled_runs = [styled(bytes)].into();
            let unfilled = quads(&selection, &mut fonts);
            selection.styled_runs[0].fill_line_width = true;
            let filled = quads(&selection, &mut fonts);
            assert_eq!(unfilled.len(), 3, "{text:?}");
            assert_eq!(filled.len(), 3, "{text:?}");
            let right = selection.pixel_x + width;
            let (first, middle, last) = (&filled[0], &filled[1], &filled[2]);
            assert!(first.y < middle.y && middle.y < last.y);
            assert!((first.x - unfilled[0].x).abs() < 0.01);
            assert!((first.x + first.w - right).abs() < 0.01);
            assert!((middle.x - selection.pixel_x).abs() < 0.01);
            assert!((middle.w - width).abs() < 0.01);
            assert!(middle.w > unfilled[1].w);
            assert!((last.x - selection.pixel_x).abs() < 0.01);
            assert!((last.x + last.w - unfilled[2].x - unfilled[2].w).abs() < 0.01);
        }
    }
}
