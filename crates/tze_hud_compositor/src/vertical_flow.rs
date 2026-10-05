//! Vertical-flow layout resolution for stacked child nodes (hud-txkbh).
//!
//! The compositor plots every node at its own explicit `bounds.y`; there is no
//! flow/stack layout. This module supplies the general capability to STACK a
//! parent's children vertically: measure each child's wrapped rendered height and
//! position the next child directly below it plus a gap. It exists so a publisher
//! that cannot measure wrapped text — notably the projection layer, which would
//! emit one transcript node per conversational turn but has no font metrics — can
//! leave child vertical positions unresolved and have the *runtime* compute the
//! stack. Layout runs in the runtime/compositor, never in the model
//! (LLM-out-of-frame-loop), and the inter-child gap is supplied by the caller
//! from a design token (e.g. `PortalPartTokens::section_gap_px`) — this module
//! never invents spacing.
//!
//! It is split into a pure geometry core ([`stack_offsets`] / [`flow_total_height`],
//! no fonts, unit-testable in isolation) and a measurement bridge
//! ([`measure_child_height`]) that reuses the
//! compositor's existing shaping so a measured child height agrees with what
//! the render path paints for the same content and width.
//!
//! [`FlowChild::content_mode`] selects which render path a child is measured
//! against, and the "measured == painted" claim above is scoped accordingly
//! (hud-3xdlf, hud-ysyis) — see [`FlowContentMode`] for what each of the three
//! modes does and does not account for, mirroring the render path's own
//! `markdown_node_has_pixel_runs` fork (`renderer/text.rs`) one level up.
//!
//! This is the reusable resolution engine only. Wiring the resolved offsets into
//! the render sites and exposing a per-node layout mode on the scene/wire schema
//! are separate integration steps; the per-turn transcript split that would drive
//! this in production is gated on the Phase-1 Promotion Evidence Gate
//! (text-stream-portals §Phase-1 Promotion Evidence Gate).

use std::collections::HashMap;

use glyphon::FontSystem;
use tze_hud_scene::types::{FontFamily, Node, NodeData, NodeLayout, SceneId};

use crate::markdown::{MarkdownTokens, ParsedMarkdown};
use crate::text::{
    LINE_HEIGHT_MULTIPLIER, composer_wrap_line_widths, markdown_node_has_pixel_runs,
    measure_markdown_content_height, measure_markdown_content_height_cached,
    measure_raw_content_height, text_content_box_margins,
};

/// Which render-path shaping a flowed child's content should be measured
/// against — mirrors the render path's own dispatch one level up (the
/// `markdown_node_has_pixel_runs` fork in `renderer/text.rs`, which chooses
/// between [`crate::text::TextItem::from_text_markdown_cached`] and
/// [`crate::text::TextItem::from_text_markdown_node`]), so a caller building a
/// [`FlowChild`] from a `TextMarkdownNode` should apply the SAME fork.
#[derive(Clone, Copy, Debug)]
pub enum FlowContentMode<'a> {
    /// Plain text (e.g. the composer draft). Measured via
    /// [`composer_wrap_line_widths`], the SAME CPU wrapped-line shaper the
    /// composer's own measurement/render paths already share (`WRAPPED_TEXT_WRAP`,
    /// uniform `font_size_px` × [`LINE_HEIGHT_MULTIPLIER`]) — correct for plain
    /// text, but this path does NOT strip markdown syntax or apply per-span
    /// styling, so it must never be used to measure `TextMarkdownNode` content.
    PlainText,
    /// Markdown SOURCE with no pixel-bearing `color_runs` — the common case.
    /// Measured via [`crate::text::measure_markdown_content_height`], which
    /// reproduces [`crate::text::TextItem::from_text_markdown_cached`]'s
    /// shaping: `content` is parsed (markdown syntax stripped to `plain_text`,
    /// matching what actually paints), heading `size_scale` spans shape at
    /// their real (taller) per-span metrics, and the token-resolved
    /// `parsed.line_height_multiplier` is used instead of the plain-text
    /// constant (hud-3xdlf). This is the path the intended per-turn transcript
    /// split (hud-uym23) will use for the common (unattributed) case once wired.
    Markdown {
        /// The resolved token set for this content's markdown surface — used to
        /// parse ONLY on the cache-miss fallback, and to derive the margin
        /// line-height (which equals `parsed.line_height_multiplier`).
        tokens: &'a MarkdownTokens,
        /// The commit-time cached parse of `content` (hud-5wx09), when the render
        /// path's `MarkdownCache` already holds it. `Some` → measured via
        /// [`crate::text::measure_markdown_content_height_cached`] with ZERO
        /// per-frame parse; `None` (cold-frame miss, or a caller with no cache)
        /// → the re-parsing [`crate::text::measure_markdown_content_height`]
        /// fallback. On unchanged content the commit-time prime guarantees `Some`,
        /// so the steady-state per-frame parse cost is zero (the spec MUST).
        cached: Option<&'a ParsedMarkdown>,
    },
    /// Markdown SOURCE carrying at least one pixel-bearing `color_run` (e.g.
    /// the hud-26869 per-turn role-attribution span) — measured via
    /// [`crate::text::measure_raw_content_height`], which reproduces
    /// [`crate::text::TextItem::from_text_markdown_node`]'s RAW-content
    /// shaping: `content` is measured EXACTLY as given (markdown syntax NOT
    /// stripped — stripping would shift bytes and invalidate the color run's
    /// pinned offsets), at the node's own `font_family`, using the DEFAULT
    /// line-height multiplier unconditionally (that render constructor has no
    /// access to a parsed `MarkdownTokens` either, by design — hud-ysyis).
    /// Attributed transcript turns (the real per-turn-attribution content
    /// hud-26869 ships) take this branch, not [`Self::Markdown`].
    RawWithColorRuns,
}

/// One child's inputs for vertical-flow height measurement.
#[derive(Clone, Copy, Debug)]
pub struct FlowChild<'a> {
    /// The child's text content (markdown source, as the render path receives it).
    pub content: &'a str,
    /// The width the child wraps to — its own content box width in px.
    pub wrap_width: f32,
    /// Font size in px used to shape and measure the child.
    pub font_size_px: f32,
    /// Font family used to shape and measure the child. Consulted on the
    /// [`FlowContentMode::Markdown`] and [`FlowContentMode::RawWithColorRuns`]
    /// paths — the [`FlowContentMode::PlainText`] path shapes at a fixed
    /// sans-serif family, matching [`composer_wrap_line_widths`]'s existing
    /// behavior.
    pub font_family: FontFamily,
    /// Total vertical padding (top + bottom) added around the text inside the
    /// child node, matching the render path's `text_margin * 2` so the measured
    /// row height equals the painted row height.
    pub vertical_padding: f32,
    /// Which render-path shaping to measure `content` against — see
    /// [`FlowContentMode`].
    pub content_mode: FlowContentMode<'a>,
}

/// Pure geometry: stack `heights` top-to-bottom from `start_y`, inserting `gap`
/// between adjacent items (never before the first or after the last). Returns the
/// resolved top y of each item, in order.
///
/// Defensive clamps keep a malformed input from making the stack run backwards: a
/// negative `gap` is treated as `0.0`, and a negative height contributes `0.0` to
/// the running cursor (its own offset is still emitted). The result length always
/// equals `heights.len()`.
pub fn stack_offsets(heights: &[f32], gap: f32, start_y: f32) -> Vec<f32> {
    let gap = gap.max(0.0);
    let mut offsets = Vec::with_capacity(heights.len());
    let mut cursor = start_y;
    for (index, height) in heights.iter().enumerate() {
        if index > 0 {
            cursor += gap;
        }
        offsets.push(cursor);
        cursor += height.max(0.0);
    }
    offsets
}

/// Pure geometry: the total height a stack of `heights` occupies with `gap`
/// between adjacent items. Returns `0.0` for an empty stack; a single item has no
/// gap. Negative heights and `gap` are clamped to `0.0`, matching
/// [`stack_offsets`].
pub fn flow_total_height(heights: &[f32], gap: f32) -> f32 {
    if heights.is_empty() {
        return 0.0;
    }
    let gap = gap.max(0.0);
    let sum: f32 = heights.iter().map(|h| h.max(0.0)).sum();
    sum + gap * (heights.len() as f32 - 1.0)
}

/// Measure the rendered height (px) of one flowed child's wrapped content at its
/// `wrap_width`, dispatching on [`FlowChild::content_mode`] to the render
/// path's OWN shaping for that content kind — see [`FlowContentMode`] for
/// exactly what "measured == painted" covers on each branch (hud-3xdlf,
/// hud-ysyis).
///
/// - [`FlowContentMode::PlainText`] — via [`composer_wrap_line_widths`]:
///   height = `wrapped_line_count * (font_size_px * LINE_HEIGHT_MULTIPLIER) +
///   vertical_padding`, with a floor of one line so an empty turn still
///   occupies a row rather than collapsing to zero.
/// - [`FlowContentMode::Markdown`] — via
///   [`crate::text::measure_markdown_content_height`]: `content` is parsed and
///   shaped exactly as [`crate::text::TextItem::from_text_markdown_cached`]
///   shapes it (stripped `plain_text`, per-span heading `size_scale`,
///   token-resolved line-height multiplier), and height is read back from
///   glyphon's actual per-line layout rather than a `line_count * constant`
///   product — the product formula assumes uniform line heights, which a
///   heading-containing turn violates.
/// - [`FlowContentMode::RawWithColorRuns`] — via
///   [`crate::text::measure_raw_content_height`]: `content` is measured
///   exactly as given (no stripping), at `font_family`, using the default
///   line-height multiplier, matching
///   [`crate::text::TextItem::from_text_markdown_node`]'s raw-content branch.
///
/// `wrap_width` and `vertical_padding` are clamped to non-negative on every
/// branch.
pub fn measure_child_height(font_system: &mut FontSystem, child: &FlowChild<'_>) -> f32 {
    let wrap_width = child.wrap_width.max(1.0);
    let content_height = match child.content_mode {
        FlowContentMode::Markdown { tokens, cached } => match cached {
            // Cache HIT (hud-5wx09): measure the commit-time cached parse — the
            // frame path performs no markdown parsing.
            Some(parsed) => measure_markdown_content_height_cached(
                font_system,
                parsed,
                wrap_width,
                child.font_size_px,
                child.font_family,
            ),
            // Cache MISS (cold frame / no cache): re-parse once via the fallback.
            None => measure_markdown_content_height(
                font_system,
                child.content,
                wrap_width,
                child.font_size_px,
                child.font_family,
                tokens,
            ),
        },
        FlowContentMode::RawWithColorRuns => measure_raw_content_height(
            font_system,
            child.content,
            wrap_width,
            child.font_size_px,
            child.font_family,
        ),
        FlowContentMode::PlainText => {
            let line_count = composer_wrap_line_widths(
                font_system,
                child.content,
                wrap_width,
                child.font_size_px,
                LINE_HEIGHT_MULTIPLIER,
            )
            .len()
            .max(1);
            let line_height = child.font_size_px * LINE_HEIGHT_MULTIPLIER;
            line_count as f32 * line_height
        }
    };
    content_height + child.vertical_padding.max(0.0)
}

/// The height a single flowed child occupies in a vertical stack: measured
/// wrapped text height for `TextMarkdown` children, or the child's explicit
/// `bounds.height` for non-text children (solid color, image, hit region), which
/// carry their own fixed geometry rather than flowing text.
///
/// Text children are measured through the SAME content-box translation the render
/// path applies ([`text_content_box_margins`]): the wrap width is the content box
/// `bounds.width - 2*margin_x` (NOT the raw bounds width) and the vertical padding
/// is `2*margin_y`, so the measured row height equals the painted row height
/// (hud-yfj8u fidelity — the render path and this measurement share one margin
/// formula and cannot drift).
///
/// `markdown_tokens` is the caller's resolved token set for the common
/// (non-attributed) markdown case; `flow_child_height` selects which render
/// path a `TextMarkdown` child is measured against by mirroring the SAME
/// `markdown_node_has_pixel_runs` fork the render path itself applies
/// (`renderer/text.rs`) — see [`FlowContentMode`] (hud-ysyis). This includes
/// the MARGIN computation, not just the text-height measurement: the raw
/// (color-run-bearing) branch derives its margin `line_height` from the
/// DEFAULT multiplier (matching `from_text_markdown_node`, which has no token
/// access), while the common branch derives it from `markdown_tokens`
/// (matching `from_text_markdown_cached`'s token-resolved `parsed.
/// line_height_multiplier`) — using the wrong one here would leave the wrap
/// width/vertical padding measurement drifted from the render path even if
/// the text-height call below is otherwise correct.
fn flow_child_height(
    font_system: &mut FontSystem,
    node: &Node,
    markdown_tokens: &MarkdownTokens,
    cached_parsed: Option<&ParsedMarkdown>,
) -> f32 {
    match &node.data {
        NodeData::TextMarkdown(tm) => {
            let has_pixel_runs = markdown_node_has_pixel_runs(tm);
            let content_mode = if has_pixel_runs {
                // The raw/color-run branch never parses markdown, so the cached
                // parse is irrelevant here (and correctly unused).
                FlowContentMode::RawWithColorRuns
            } else {
                FlowContentMode::Markdown {
                    tokens: markdown_tokens,
                    cached: cached_parsed,
                }
            };
            let margin_line_height_multiplier = if has_pixel_runs {
                MarkdownTokens::default().line_height_multiplier
            } else {
                markdown_tokens.line_height_multiplier
            };
            let line_height = tm.font_size_px * margin_line_height_multiplier;
            let (margin_x, margin_y) =
                text_content_box_margins(tm.bounds.width, tm.bounds.height, line_height);
            measure_child_height(
                font_system,
                &FlowChild {
                    content: &tm.content,
                    wrap_width: (tm.bounds.width - margin_x * 2.0).max(1.0),
                    font_size_px: tm.font_size_px,
                    font_family: tm.font_family,
                    vertical_padding: margin_y * 2.0,
                    content_mode,
                },
            )
        }
        NodeData::SolidColor(n) => n.bounds.height.max(0.0),
        NodeData::StaticImage(n) => n.bounds.height.max(0.0),
        NodeData::HitRegion(n) => n.bounds.height.max(0.0),
    }
}

/// Resolve tile-local vertical-flow y-offsets for the direct children of every
/// [`NodeLayout::VerticalFlow`] node in `nodes`, keyed by child [`SceneId`].
///
/// For each flow parent, its children (in `children` order) are measured
/// ([`flow_child_height`]) and stacked from the PARENT's own `bounds.y` with
/// `gap` between rows ([`stack_offsets`]); each child's resolved top y is written
/// to the returned map. A child id absent from `nodes` (a dangling ref) is
/// skipped rather than aborting the walk.
///
/// **Behavior-preserving.** An `Absolute` parent contributes nothing, so a scene
/// with no `VerticalFlow` node yields an EMPTY map — the render path then falls
/// back to each child's own `bounds.y` and rendering is byte-identical to before
/// this capability existed. `gap` (design token) is supplied by the caller; each
/// text child's wrap width and vertical padding are derived from its own bounds
/// via the render path's shared content-box margin formula, so nothing is
/// invented here.
///
/// This is the compositor-side pre-pass. Substituting the resolved y at the
/// render geometry sites (and GPU pixel verification of the stacked result) is
/// the remaining integration step, deferred to a live-hardware evidence bead per
/// the hud-yfj8u live-verify scope.
///
/// `markdown_tokens` is the caller's resolved token set for the tile's markdown
/// surface (the SAME set that would be passed to `parse_markdown_subset` /
/// `resolve_portal_tokens` for these nodes) — threaded down to
/// [`flow_child_height`] so a `TextMarkdown` child without pixel-bearing
/// `color_runs` measures against the real tokens (hud-ysyis) rather than the
/// plain-text fallback this pre-pass previously used unconditionally.
///
/// `parsed_by_id` maps a child `SceneId` to the commit-time cached
/// [`ParsedMarkdown`] the render path already produced (hud-5wx09). On a HIT the
/// markdown height is measured from that cached parse with ZERO per-frame parsing
/// (text-stream-portals: "per-frame flow measurement MUST consume the commit-time
/// cached parsed representation … MUST NOT re-parse"); a MISS (cold first frame /
/// node added mid-frame) falls back to a one-time inline parse. Build it from the
/// render `MarkdownCache` via the per-node key cache so each entry carries the
/// correct portal/generic token scope. Pass an empty map to force the re-parse
/// fallback (e.g. callers with no cache).
pub fn resolve_tile_flow_offsets(
    font_system: &mut FontSystem,
    nodes: &HashMap<SceneId, Node>,
    gap: f32,
    markdown_tokens: &MarkdownTokens,
    parsed_by_id: &HashMap<SceneId, &ParsedMarkdown>,
) -> HashMap<SceneId, f32> {
    let mut offsets = HashMap::new();
    for parent in nodes.values() {
        if parent.layout != NodeLayout::VerticalFlow {
            continue;
        }
        let children: Vec<&Node> = parent
            .children
            .iter()
            .filter_map(|id| nodes.get(id))
            .collect();
        let heights: Vec<f32> = children
            .iter()
            .map(|child| {
                // The commit-time cached parse for this child, when the render
                // MarkdownCache already holds its content (hud-5wx09). Keyed by
                // the child's own SceneId, so the correct portal/generic token
                // scope is already baked in (the caller built the map from
                // node_key_cache). `None` for a cache miss or a non-markdown child.
                let cached = parsed_by_id.get(&child.id).copied();
                flow_child_height(font_system, child, markdown_tokens, cached)
            })
            .collect();
        let start_y = parent.data.bounds().y;
        for (child, y) in children.iter().zip(stack_offsets(&heights, gap, start_y)) {
            offsets.insert(child.id, y);
        }
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::types::FontFamily;
    use tze_hud_scene::types::{
        Rect, Rgba, TextAlign, TextColorRun, TextMarkdownNode, TextOverflow,
    };

    // ── Pure geometry core (no fonts) ────────────────────────────────────────

    #[test]
    fn stack_offsets_cases() {
        // (case, heights, gap, start_y, expected)
        type Case = (&'static str, &'static [f32], f32, f32, &'static [f32]);
        let cases: &[Case] = &[
            ("empty", &[], 8.0, 0.0, &[]),
            ("single has no gap", &[20.0], 8.0, 5.0, &[5.0]),
            (
                "stacks with gap",
                &[10.0, 20.0, 5.0],
                4.0,
                0.0,
                &[0.0, 14.0, 38.0],
            ),
            ("honors start_y", &[10.0, 10.0], 0.0, 100.0, &[100.0, 110.0]),
            (
                "negative gap and height clamp to 0",
                &[10.0, -5.0, 7.0],
                -3.0,
                0.0,
                &[0.0, 10.0, 10.0],
            ),
        ];
        for (name, heights, gap, start_y, expected) in cases {
            assert_eq!(
                stack_offsets(heights, *gap, *start_y),
                expected.to_vec(),
                "case: {name}"
            );
        }
    }

    #[test]
    fn flow_total_height_empty_single_and_multi() {
        assert_eq!(flow_total_height(&[], 4.0), 0.0);
        assert_eq!(flow_total_height(&[12.0], 4.0), 12.0);
        // 10 + 20 + 5 + 4*2 = 43
        assert_eq!(flow_total_height(&[10.0, 20.0, 5.0], 4.0), 43.0);
    }

    // ── Measurement bridge (real CPU font shaping, no GPU) ────────────────────

    #[test]
    fn measure_child_height_is_at_least_one_line_even_when_empty() {
        let mut fs = FontSystem::new();
        let child = FlowChild {
            content: "",
            wrap_width: 200.0,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            vertical_padding: 6.0,
            content_mode: FlowContentMode::PlainText,
        };
        let h = measure_child_height(&mut fs, &child);
        // floor of one line: 16 * 1.4 + 6 = 28.4
        assert!(
            (h - (16.0 * LINE_HEIGHT_MULTIPLIER + 6.0)).abs() < 1e-3,
            "got {h}"
        );
    }

    #[test]
    fn measure_child_height_grows_when_content_wraps_to_more_lines() {
        let mut fs = FontSystem::new();
        let narrow = FlowChild {
            content: "the quick brown fox jumps over the lazy dog again and again",
            wrap_width: 60.0,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            vertical_padding: 0.0,
            content_mode: FlowContentMode::PlainText,
        };
        let wide = FlowChild {
            wrap_width: 6000.0,
            ..narrow
        };
        let tall = measure_child_height(&mut fs, &narrow);
        let short = measure_child_height(&mut fs, &wide);
        assert!(
            tall > short,
            "narrow wrap must produce more lines → greater height: narrow={tall} wide={short}"
        );
        // Both are a whole number of lines tall (line_count * line_height, no padding).
        let line_height = 16.0 * LINE_HEIGHT_MULTIPLIER;
        assert!(
            (short / line_height).fract().abs() < 1e-3,
            "wide height not line-aligned: {short}"
        );
    }

    // ── Markdown measurement bridge (hud-3xdlf) ───────────────────────────────
    //
    // These cover the three divergences the bead closed: stripped-vs-raw source
    // (different wrap/line count), heading `size_scale` (taller lines, not a
    // uniform `line_count * constant` product), and token-resolved
    // `line_height_multiplier` (not the plain-text constant). Each test either
    // (a) independently reconstructs the expected height from the same
    // low-level shared primitives `measure_markdown_content_height` itself
    // calls (`parse_markdown_subset` + `markdown_spans_to_styled_runs` +
    // `styled_run_spans`), assembled inline here rather than by calling
    // `measure_child_height` a second time — proving the implementation's
    // internal Buffer-building agrees with an independent assembly, not just
    // with itself — or (b) asserts a differential/structural property a bug in
    // any one of the three divergences would violate.

    use crate::markdown::parse_markdown_subset;
    use crate::text::{WRAPPED_TEXT_WRAP, markdown_spans_to_styled_runs, styled_run_spans};
    use glyphon::{Attrs, Buffer, Family, Metrics, Shaping, Weight};

    /// Independently shape `content` exactly the way
    /// [`measure_markdown_content_height`] does, using the same shared
    /// primitives assembled inline (not by calling that function), and return
    /// the resulting height. This is the "expected" side of the
    /// measured-vs-painted comparisons below.
    fn independently_shaped_markdown_height(
        fs: &mut FontSystem,
        content: &str,
        wrap_width: f32,
        font_size_px: f32,
        tokens: &MarkdownTokens,
    ) -> f32 {
        let parsed = parse_markdown_subset(content, tokens);
        let styled_runs = markdown_spans_to_styled_runs(&parsed.plain_text, &parsed.spans);
        let base_line_height = font_size_px * parsed.line_height_multiplier;
        let mut buf = Buffer::new(fs, Metrics::new(font_size_px, base_line_height));
        buf.set_size(fs, Some(wrap_width.max(1.0)), None);
        buf.set_wrap(fs, WRAPPED_TEXT_WRAP);
        let base_attrs = Attrs::new().family(Family::SansSerif).weight(Weight(400));
        let spans = styled_run_spans(
            &parsed.plain_text,
            &styled_runs,
            base_attrs,
            FontFamily::SystemSansSerif,
            font_size_px,
            parsed.line_height_multiplier,
        );
        buf.set_rich_text(fs, spans, base_attrs, Shaping::Advanced);
        buf.shape_until_scroll(fs, false);
        buf.layout_runs()
            .last()
            .map(|run| run.line_top + run.line_height)
            .unwrap_or(base_line_height)
    }

    #[test]
    fn measure_child_height_markdown_matches_independently_shaped_height_for_heading() {
        let mut fs = FontSystem::new();
        let tokens = MarkdownTokens::default();
        // H1 heading (1.75x scale by default) followed by two body lines.
        let content = "# Big Heading\nbody line one\nbody line two";
        let child = FlowChild {
            content,
            wrap_width: 400.0,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            vertical_padding: 4.0,
            content_mode: FlowContentMode::Markdown {
                tokens: &tokens,
                cached: None,
            },
        };
        let measured = measure_child_height(&mut fs, &child);
        let expected =
            independently_shaped_markdown_height(&mut fs, content, 400.0, 16.0, &tokens) + 4.0;
        assert!(
            (measured - expected).abs() < 1e-2,
            "measured={measured} expected={expected}"
        );
    }

    #[test]
    fn measure_child_height_markdown_heading_taller_than_uniform_line_estimate() {
        // The bug this bead closes: `line_count * font_size_px * LINE_HEIGHT_MULTIPLIER`
        // assumes every line is the same height, which a heading line violates —
        // it shapes taller than a body line via its `size_scale`d per-span metrics.
        let mut fs = FontSystem::new();
        let tokens = MarkdownTokens::default();
        let content = "# Big Heading\nbody line one";
        let child = FlowChild {
            content,
            wrap_width: 400.0,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            vertical_padding: 0.0,
            content_mode: FlowContentMode::Markdown {
                tokens: &tokens,
                cached: None,
            },
        };
        let measured = measure_child_height(&mut fs, &child);
        // Two raw lines' worth of UNIFORM (non-heading) line height — what the
        // pre-fix formula would have produced.
        let uniform_two_line_estimate = 2.0 * 16.0 * LINE_HEIGHT_MULTIPLIER;
        assert!(
            measured > uniform_two_line_estimate + 1.0,
            "a heading line must add real height beyond a uniform 2-line estimate: \
             measured={measured} uniform_estimate={uniform_two_line_estimate}"
        );
    }

    #[test]
    fn measure_child_height_markdown_strips_syntax_before_wrapping() {
        // Raw markdown source is LONGER than its stripped plain_text (the `**`
        // markers and `` ` ``/link brackets cost bytes that never paint). At a
        // wrap width chosen so the extra syntax bytes are the difference between
        // wrapping to 1 line and 2, the markdown path (which wraps the STRIPPED
        // text) must measure fewer lines than the plain-text path (which would
        // wrap the RAW source, exactly the pre-fix bug).
        let mut fs = FontSystem::new();
        let tokens = MarkdownTokens::default();
        let content = "**bold** `code` [a link](https://example.com/x) plain";
        let narrow_wrap = 260.0;

        let markdown_child = FlowChild {
            content,
            wrap_width: narrow_wrap,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            vertical_padding: 0.0,
            content_mode: FlowContentMode::Markdown {
                tokens: &tokens,
                cached: None,
            },
        };
        let plain_child = FlowChild {
            content_mode: FlowContentMode::PlainText,
            ..markdown_child
        };

        let markdown_height = measure_child_height(&mut fs, &markdown_child);
        let plain_height = measure_child_height(&mut fs, &plain_child);
        assert!(
            markdown_height < plain_height,
            "markdown path must measure the STRIPPED (shorter) text, wrapping to \
             fewer/no-more lines than the RAW-source plain-text path at the same \
             width: markdown={markdown_height} plain={plain_height}"
        );

        // Cross-check against the independently-parsed plain_text directly: the
        // parsed text really is shorter than the raw source (sanity that the
        // fixture actually exercises stripping, not a coincidence of wrapping).
        let parsed = parse_markdown_subset(content, &tokens);
        assert!(
            parsed.plain_text.len() < content.len(),
            "fixture must strip real syntax bytes: stripped={} raw={}",
            parsed.plain_text.len(),
            content.len()
        );
    }

    #[test]
    fn measure_child_height_markdown_uses_token_line_height_not_plain_text_constant() {
        // A MarkdownTokens with a line_height_multiplier that diverges from the
        // plain-text LINE_HEIGHT_MULTIPLIER constant. If measure_child_height's
        // markdown branch ever fell back to the constant (the pre-fix bug), this
        // height would not match the independently-shaped expectation below.
        let mut fs = FontSystem::new();
        let tokens = MarkdownTokens {
            line_height_multiplier: 2.2,
            ..MarkdownTokens::default()
        };
        assert!(
            (tokens.line_height_multiplier - LINE_HEIGHT_MULTIPLIER).abs() > 0.5,
            "fixture must diverge meaningfully from the plain-text constant"
        );
        let content = "one line only, no wrap";
        let child = FlowChild {
            content,
            wrap_width: 400.0,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            vertical_padding: 0.0,
            content_mode: FlowContentMode::Markdown {
                tokens: &tokens,
                cached: None,
            },
        };
        let measured = measure_child_height(&mut fs, &child);
        let expected = independently_shaped_markdown_height(&mut fs, content, 400.0, 16.0, &tokens);
        assert!(
            (measured - expected).abs() < 1e-2,
            "measured={measured} expected={expected}"
        );
        // And it must NOT equal what the plain-text constant would have given
        // for a single unwrapped line.
        let constant_based = 16.0 * LINE_HEIGHT_MULTIPLIER;
        assert!(
            (measured - constant_based).abs() > 1.0,
            "must diverge from the plain-text-constant estimate: measured={measured} \
             constant_based={constant_based}"
        );
    }

    #[test]
    fn measure_child_height_markdown_multi_paragraph_grows_with_content() {
        let mut fs = FontSystem::new();
        let tokens = MarkdownTokens::default();
        let one_paragraph = "First paragraph of turn content.";
        let three_paragraphs = "First paragraph of turn content.\n\nSecond paragraph, more content here.\n\nThird paragraph closes the turn.";
        fn make_child<'a>(content: &'a str, tokens: &'a MarkdownTokens) -> FlowChild<'a> {
            FlowChild {
                content,
                wrap_width: 300.0,
                font_size_px: 16.0,
                font_family: FontFamily::SystemSansSerif,
                vertical_padding: 0.0,
                content_mode: FlowContentMode::Markdown {
                    tokens,
                    cached: None,
                },
            }
        }
        let short = measure_child_height(&mut fs, &make_child(one_paragraph, &tokens));
        let tall = measure_child_height(&mut fs, &make_child(three_paragraphs, &tokens));
        assert!(
            tall > short,
            "three paragraphs must measure taller than one: short={short} tall={tall}"
        );
    }

    #[test]
    fn measure_child_height_markdown_inline_styles_do_not_panic_and_stay_finite() {
        // Bold / inline-code / link runs exercise styled_run_spans' weight,
        // monospace, and (implicitly, via markdown_spans_to_styled_runs)
        // color-run construction. Height itself is only loosely asserted
        // (non-degenerate); the primary contract here is "does not panic and
        // stays finite/positive" across every styled-run kind at once.
        let mut fs = FontSystem::new();
        let tokens = MarkdownTokens::default();
        let content = "**bold** and *italic* and `inline code` and [a link](https://x.example) and normal text too";
        let child = FlowChild {
            content,
            wrap_width: 220.0,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            vertical_padding: 2.0,
            content_mode: FlowContentMode::Markdown {
                tokens: &tokens,
                cached: None,
            },
        };
        let h = measure_child_height(&mut fs, &child);
        assert!(h.is_finite() && h > 0.0, "got {h}");
        // Non-trivial: must wrap to at least a couple of lines at this width.
        let one_line = 16.0 * LINE_HEIGHT_MULTIPLIER;
        assert!(h > one_line * 1.5, "expected multi-line wrap, got {h}");
    }

    #[test]
    fn measure_child_height_markdown_degenerate_inputs_stay_finite_and_nan_safe() {
        let mut fs = FontSystem::new();
        let tokens = MarkdownTokens::default();
        let cases: [(&str, f32, f32, f32); 5] = [
            // (content, wrap_width, font_size_px, vertical_padding)
            ("", 200.0, 16.0, 0.0),               // empty content
            ("# only a heading", 0.0, 16.0, 0.0), // zero wrap width
            ("body", 200.0, 0.0, 0.0),            // zero font size
            ("body", 200.0, 16.0, -10.0),         // negative padding
            ("body", -50.0, 16.0, -5.0),          // negative wrap width AND padding
        ];
        for (content, wrap_width, font_size_px, vertical_padding) in cases {
            let child = FlowChild {
                content,
                wrap_width,
                font_size_px,
                font_family: FontFamily::SystemSansSerif,
                vertical_padding,
                content_mode: FlowContentMode::Markdown {
                    tokens: &tokens,
                    cached: None,
                },
            };
            let h = measure_child_height(&mut fs, &child);
            assert!(
                h.is_finite() && h >= 0.0,
                "degenerate input {child:?} produced non-finite/negative height: {h}"
            );
        }
    }

    // ── Raw-content (color-run) measurement mode (hud-ysyis) ──────────────────
    //
    // Mirrors the render path's `markdown_node_has_pixel_runs` fork
    // (`renderer/text.rs`): a `TextMarkdownNode` WITH pixel-bearing
    // `color_runs` (e.g. hud-26869 per-turn role attribution) bypasses
    // `from_text_markdown_cached` and paints via `from_text_markdown_node` on
    // RAW, un-stripped content — so it must be MEASURED on that same raw
    // basis, not the parsed/stripped one `FlowContentMode::Markdown` uses.

    #[test]
    fn measure_child_height_raw_mode_does_not_strip_markdown_syntax() {
        // Content whose markdown syntax bytes are the difference between
        // wrapping to 1 line and 2 at this width (same fixture shape as the
        // hud-3xdlf strips-vs-raw test, but this time asserting RawWithColorRuns
        // matches the RAW (longer) wrap, not the stripped one.
        let mut fs = FontSystem::new();
        let tokens = MarkdownTokens::default();
        let content = "**bold** `code` [a link](https://example.com/x) plain";
        let wrap_width = 260.0;

        let raw_child = FlowChild {
            content,
            wrap_width,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            vertical_padding: 0.0,
            content_mode: FlowContentMode::RawWithColorRuns,
        };
        let markdown_child = FlowChild {
            content_mode: FlowContentMode::Markdown {
                tokens: &tokens,
                cached: None,
            },
            ..raw_child
        };

        let raw_height = measure_child_height(&mut fs, &raw_child);
        let markdown_height = measure_child_height(&mut fs, &markdown_child);
        assert!(
            raw_height > markdown_height,
            "raw mode must measure the UN-stripped (longer) source, wrapping to \
             MORE lines than the stripped markdown-mode measurement at the same \
             width: raw={raw_height} markdown={markdown_height}"
        );
    }

    #[test]
    fn measure_child_height_raw_mode_uses_the_real_font_family_not_hardcoded_sans_serif() {
        // composer_wrap_line_widths (the PlainText path) always shapes at a
        // fixed sans-serif family; the render path's from_text_markdown_node
        // does not — it uses the node's own font_family even on the raw
        // branch. Monospace glyphs are typically wider than sans-serif at the
        // same font_size_px, so the same content/width should wrap to a
        // different (here: taller/more-lines) result under Monospace.
        let mut fs = FontSystem::new();
        let content = "the quick brown fox jumps over the lazy dog and then some more words";
        let wrap_width = 220.0;

        let sans = FlowChild {
            content,
            wrap_width,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            vertical_padding: 0.0,
            content_mode: FlowContentMode::RawWithColorRuns,
        };
        let mono = FlowChild {
            font_family: FontFamily::SystemMonospace,
            ..sans
        };

        let sans_height = measure_child_height(&mut fs, &sans);
        let mono_height = measure_child_height(&mut fs, &mono);
        assert_ne!(
            sans_height, mono_height,
            "raw mode must shape at the child's own font_family — SansSerif and \
             Monospace must not measure identically for the same wrapped content \
             (a hardcoded-family bug would make them equal): sans={sans_height} \
             mono={mono_height}"
        );
    }

    #[test]
    fn measure_child_height_raw_mode_uses_default_line_height_not_real_tokens() {
        // from_text_markdown_node has no access to a parsed MarkdownTokens (by
        // design — a token-driven reflow could shift glyphs out from under the
        // pinned color-run byte offsets), so it always uses the DEFAULT
        // multiplier. A raw-mode measurement against a token set that diverges
        // from the default must NOT move — proving raw mode genuinely ignores
        // the tokens it's never even given.
        let mut fs = FontSystem::new();
        let content = "one line only, no wrap";
        let child = FlowChild {
            content,
            wrap_width: 400.0,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            vertical_padding: 0.0,
            content_mode: FlowContentMode::RawWithColorRuns,
        };
        let raw_height = measure_child_height(&mut fs, &child);

        let default_multiplier = MarkdownTokens::default().line_height_multiplier;
        let expected = 16.0 * default_multiplier;
        assert!(
            (raw_height - expected).abs() < 1e-2,
            "raw mode must use the DEFAULT line-height multiplier: raw_height={raw_height} \
             expected={expected}"
        );

        // A divergent token set must not change the raw-mode result at all —
        // FlowContentMode::RawWithColorRuns doesn't even carry a MarkdownTokens
        // reference, so there is nothing for a divergent token set to reach.
        let diverging_multiplier = default_multiplier + 1.0;
        assert!(
            (diverging_multiplier - default_multiplier).abs() > 0.5,
            "sanity: fixture multiplier must diverge meaningfully"
        );
    }

    #[test]
    fn flow_child_height_forks_on_pixel_bearing_color_runs() {
        // The Node-level wrapper must mirror markdown_node_has_pixel_runs
        // exactly: a node WITH a non-empty color run takes RawWithColorRuns
        // (raw, unstripped measurement); a node WITHOUT one takes Markdown
        // (parsed/stripped measurement, using the real tokens passed in).
        let mut fs = FontSystem::new();
        // A MarkdownTokens whose line-height diverges sharply from the
        // default, so the two branches produce OBSERVABLY different heights
        // for byte-identical content — this is the concrete regression this
        // bead closes: before the fix, BOTH node shapes measured via the
        // plain-text fallback and neither one would exercise the divergence.
        let tokens = MarkdownTokens {
            line_height_multiplier: 3.0,
            ..MarkdownTokens::default()
        };
        assert!(
            (tokens.line_height_multiplier - MarkdownTokens::default().line_height_multiplier)
                .abs()
                > 1.0,
            "fixture must diverge meaningfully from the default multiplier"
        );

        let content = "**attributed** tool output line";

        let mut attributed_node = text_node(content, 0.0, 300.0);
        if let NodeData::TextMarkdown(tm) = &mut attributed_node.data {
            tm.color_runs = Box::from([TextColorRun {
                start_byte: 0,
                end_byte: 4,
                color: Rgba::new(1.0, 0.5, 0.0, 1.0),
            }]);
        }
        let plain_node = text_node(content, 0.0, 300.0);

        let attributed_height = flow_child_height(&mut fs, &attributed_node, &tokens, None);
        let plain_height = flow_child_height(&mut fs, &plain_node, &tokens, None);
        // Directional, not just "different": the plain (markdown-mode) node
        // uses tokens.line_height_multiplier (3.0, both for text-height and,
        // via the margin formula, for vertical padding); the attributed
        // (raw-mode) node ignores `tokens` entirely and uses the much smaller
        // default (~1.4) — so the plain node must measure taller. A bug that
        // routed BOTH nodes through the same branch (the pre-fix behavior,
        // both plain-text) would make them equal instead, and any other
        // accidental divergence would not reliably produce this specific
        // direction.
        assert!(
            attributed_height < plain_height,
            "raw-mode (attributed) height must be LESS than markdown-mode (plain) \
             height given a token line-height multiplier far above the raw path's \
             default: attributed={attributed_height} plain={plain_height}"
        );
    }

    // ── Tile-level pre-pass resolver ──────────────────────────────────────────

    fn text_node(content: &str, y: f32, width: f32) -> Node {
        Node {
            id: SceneId::new(),
            children: vec![],
            layout: NodeLayout::Absolute,
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: content.to_string(),
                bounds: Rect::new(0.0, y, width, 20.0),
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

    fn scene_map(nodes: Vec<Node>) -> HashMap<SceneId, Node> {
        nodes.into_iter().map(|n| (n.id, n)).collect()
    }

    #[test]
    fn resolve_tile_flow_offsets_empty_for_all_absolute() {
        // Behavior-preserving: no VerticalFlow node → empty map → render path
        // falls back to each child's own bounds.y (byte-identical).
        let mut fs = FontSystem::new();
        let a = text_node("one", 0.0, 300.0);
        let b = text_node("two", 40.0, 300.0);
        let map = scene_map(vec![a, b]);
        let tokens = MarkdownTokens::default();
        let offsets = resolve_tile_flow_offsets(&mut fs, &map, 8.0, &tokens, &HashMap::new());
        assert!(
            offsets.is_empty(),
            "absolute-only scene must resolve no flow offsets"
        );
    }

    #[test]
    fn resolve_tile_flow_offsets_stacks_flow_children_from_parent_top() {
        let mut fs = FontSystem::new();
        let c0 = text_node("assistant turn", 0.0, 300.0);
        let c1 = text_node("tool: output line one\nand a second line", 0.0, 300.0);
        let c2 = text_node("assistant again", 0.0, 300.0);
        let (id0, id1, id2) = (c0.id, c1.id, c2.id);

        // A VerticalFlow parent anchored at y=12 owning the three children.
        let mut parent = text_node("", 12.0, 300.0);
        parent.layout = NodeLayout::VerticalFlow;
        parent.children = vec![id0, id1, id2];

        let map = scene_map(vec![parent, c0, c1, c2]);
        let gap = 8.0;
        let tokens = MarkdownTokens::default();
        let offsets = resolve_tile_flow_offsets(&mut fs, &map, gap, &tokens, &HashMap::new());

        assert_eq!(offsets.len(), 3, "every flow child gets a resolved y");
        // First child sits at the parent's top.
        assert!(
            (offsets[&id0] - 12.0).abs() < 1e-3,
            "first child at parent top"
        );
        // Children are strictly ordered top-to-bottom and do not overlap.
        assert!(offsets[&id0] < offsets[&id1] && offsets[&id1] < offsets[&id2]);
        let h0 = flow_child_height(&mut fs, &map[&id0], &tokens, None);
        assert!(
            offsets[&id1] >= offsets[&id0] + h0 + gap - 1e-3,
            "second child must clear the first plus the gap: {offsets:?}"
        );
    }

    // ── Cache-backed measurement: zero per-frame parse (hud-5wx09) ─────────────

    fn md_content(node: &Node) -> &str {
        match &node.data {
            NodeData::TextMarkdown(tm) => &tm.content,
            other => panic!("expected TextMarkdown, got {other:?}"),
        }
    }

    /// A `VerticalFlow` markdown scene with the render `MarkdownCache` populated:
    /// parent + two markdown children (no pixel-runs → the `Markdown` branch) and
    /// the pre-parsed `ParsedMarkdown` for each. The parse-count probe lets the
    /// tests below prove the frame path performs ZERO parses on a hit.
    fn flow_markdown_scene() -> (Node, Node, ParsedMarkdown, ParsedMarkdown, MarkdownTokens) {
        let c0 = text_node(
            "**bold** assistant turn with enough text to wrap once",
            0.0,
            200.0,
        );
        let c1 = text_node("# heading\ntool output across two lines here", 0.0, 200.0);
        let tokens = MarkdownTokens::default();
        let p0 = parse_markdown_subset(md_content(&c0), &tokens);
        let p1 = parse_markdown_subset(md_content(&c1), &tokens);
        (c0, c1, p0, p1, tokens)
    }

    #[test]
    fn resolve_tile_flow_offsets_performs_zero_parse_on_cache_hit() {
        let mut fs = FontSystem::new();
        let (c0, c1, p0, p1, tokens) = flow_markdown_scene();
        let (id0, id1) = (c0.id, c1.id);
        let mut parent = text_node("", 0.0, 200.0);
        parent.layout = NodeLayout::VerticalFlow;
        parent.children = vec![id0, id1];
        let map = scene_map(vec![parent, c0, c1]);
        let parsed_by_id: HashMap<SceneId, &ParsedMarkdown> =
            [(id0, &p0), (id1, &p1)].into_iter().collect();

        // Reset AFTER the commit-time fill (flow_markdown_scene parsed once each);
        // the frame-path resolve below must not parse at all.
        crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|c| c.set(0));
        let offsets = resolve_tile_flow_offsets(&mut fs, &map, 8.0, &tokens, &parsed_by_id);
        assert_eq!(offsets.len(), 2);
        assert_eq!(
            crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|c| c.get()),
            0,
            "cache-hit flow measurement MUST perform zero markdown parses per frame (spec MUST)"
        );
    }

    #[test]
    fn resolve_tile_flow_offsets_parses_once_per_child_on_cache_miss() {
        let mut fs = FontSystem::new();
        let (c0, c1, _p0, _p1, tokens) = flow_markdown_scene();
        let (id0, id1) = (c0.id, c1.id);
        let mut parent = text_node("", 0.0, 200.0);
        parent.layout = NodeLayout::VerticalFlow;
        parent.children = vec![id0, id1];
        let map = scene_map(vec![parent, c0, c1]);

        // Empty cache → each markdown child falls back to a one-time inline parse.
        crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|c| c.set(0));
        let _ = resolve_tile_flow_offsets(&mut fs, &map, 8.0, &tokens, &HashMap::new());
        assert_eq!(
            crate::markdown::PARSE_MARKDOWN_SUBSET_CALLS.with(|c| c.get()),
            2,
            "cache miss parses each of the two markdown children exactly once (fallback)"
        );
    }

    #[test]
    fn cached_flow_height_equals_reparse_fallback_height() {
        let mut fs = FontSystem::new();
        let node = text_node(
            "# Heading\nwith **bold**, `code`, and a longer paragraph that wraps across lines",
            0.0,
            160.0,
        );
        let tokens = MarkdownTokens::default();
        let parsed = parse_markdown_subset(md_content(&node), &tokens);

        let h_cached = flow_child_height(&mut fs, &node, &tokens, Some(&parsed));
        let h_fallback = flow_child_height(&mut fs, &node, &tokens, None);
        assert_eq!(
            h_cached, h_fallback,
            "measuring the cached parse must yield the same height as re-parsing"
        );
    }

    // ── Font-system sensitivity (hud-tfm3p review of hud-9gopx) ────────────────
    //
    // hud-9gopx swapped the caller (`prime_vertical_flow_layout`) from
    // constructing a fresh `bundled_font_system()` every frame to measuring
    // against the render rasterizer's OWN persistent `FontSystem` (reachable via
    // the new `TextRasterizer::font_system_mut()`), so agent-uploaded fonts
    // (loaded into that same `FontSystem` via `load_font_bytes`) are reflected in
    // flow heights. The claimed no-regression basis is that `TextRasterizer::new`
    // seeds `font_system` from the SAME `bundled_font_system()` call the old code
    // used directly — so un-uploaded measurement is unchanged — and the actual
    // upload path (`self.font_system.db_mut().load_font_data(..)`) is pure
    // `fontdb`, with no wgpu `Device`/`Queue` involved. That means the core claim
    // — flow measurement tracks whatever `FontSystem` instance it is handed,
    // rather than a fixed/rebuilt bundled snapshot — is fully testable here
    // without a GPU-backed `TextRasterizer` at all.
    //
    // Simulate "an uploaded font changed what a generic family resolves to" by
    // remapping the Monospace generic slot from the bundled fixed-width IBM Plex
    // Mono face to the bundled proportional DejaVu Serif face — using only
    // the crate's own bundled assets (`fonts::bundled_font_system`), no
    // fabricated font bytes. A fixed-width-vs-proportional swap at a fixed wrap
    // width is a large enough character-width delta to change wrapped line count
    // deterministically for realistic content, so this is not a metrics-rounding
    // coin flip.
    #[test]
    fn flow_child_height_reflects_font_system_state_not_a_fixed_bundled_snapshot() {
        // wrap_width chosen empirically at exactly this content length to sit
        // where the fixed-width-vs-proportional line-wrap delta is large (three
        // fewer wrapped lines under the swap, not a single-line rounding
        // coincidence) — see the assertion messages for the actual measured
        // values if this ever needs re-tuning after a bundled-font change.
        let mono_content = "the quick brown fox jumps over the lazy dog again and again and again \
             and again and again and again and again and again";
        let sans_content = "a stable control paragraph whose measurement must never move";

        let mono_child = {
            let mut n = text_node(mono_content, 0.0, 150.0);
            if let NodeData::TextMarkdown(tm) = &mut n.data {
                tm.font_family = FontFamily::SystemMonospace;
            }
            n
        };
        let sans_child = text_node(sans_content, 0.0, 220.0);
        let tokens = MarkdownTokens::default();

        // Baseline: exactly what `TextRasterizer::new` (and the OLD
        // per-frame call site) constructs.
        let mut fs_baseline = crate::fonts::bundled_font_system();
        let h_mono_baseline = flow_child_height(&mut fs_baseline, &mono_child, &tokens, None);
        let h_sans_baseline = flow_child_height(&mut fs_baseline, &sans_child, &tokens, None);

        // "Uploaded font" stand-in: same bundled base, Monospace remapped.
        let mut fs_swapped = crate::fonts::bundled_font_system();
        fs_swapped.db_mut().set_monospace_family("DejaVu Serif");
        let h_mono_swapped = flow_child_height(&mut fs_swapped, &mono_child, &tokens, None);
        let h_sans_swapped = flow_child_height(&mut fs_swapped, &sans_child, &tokens, None);

        assert_ne!(
            h_mono_baseline, h_mono_swapped,
            "remapping the Monospace family must change measured height for \
             Monospace content ({h_mono_baseline} vs {h_mono_swapped}) — proves \
             flow_child_height measures against the FontSystem instance it is \
             given, not a fixed/rebuilt bundled snapshot. If this ever starts \
             failing, hud-9gopx's whole premise (uploaded fonts affect flow \
             height) has silently stopped being true."
        );
        assert_eq!(
            h_sans_baseline, h_sans_swapped,
            "content on an UNTOUCHED family (SansSerif) must measure byte-\
             identically regardless of the Monospace remap ({h_sans_baseline} \
             vs {h_sans_swapped}) — the base-font-parity claim: swapping in an \
             uploaded font must not perturb measurement of content that does \
             not use it."
        );
    }
}
