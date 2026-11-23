//! Vertical-flow layout resolution for stacked child nodes.
//!
//! `NodeLayout::VerticalFlow` stacks a parent's children by their measured
//! wrapped height plus the caller-supplied gap. Absolute-layout nodes keep their
//! explicit `bounds.y`. The compositor resolves the offsets before geometry and
//! text rendering, so publishers need no font metrics to position stacked text.
//! Layout stays in the runtime/compositor; spacing comes from design tokens.
//!
//! It is split into a pure geometry core ([`stack_offsets`] / [`flow_total_height`],
//! no fonts, unit-testable in isolation) and a measurement bridge
//! ([`measure_child_height`]) that reuses the
//! compositor's existing shaping so a measured child height agrees with what
//! the render path paints for the same content and width.
//!
//! [`FlowChild::content_mode`] selects which render path a child is measured
//! against, and the "measured == painted" claim above is scoped accordingly
//! See [`FlowContentMode`] for what each of the three
//! modes does and does not account for, mirroring the render path's own
//! `markdown_node_has_pixel_runs` fork (`renderer/text.rs`) one level up.
//!
//! `Compositor::prime_vertical_flow_layout` calls this resolver before the frame
//! geometry and text passes consume the resolved offsets.

use std::collections::HashMap;

use glyphon::FontSystem;
use tze_hud_scene::types::{FontFamily, Node, NodeData, NodeLayout, SceneId};

use crate::markdown::{MarkdownTokens, ParsedMarkdown};
#[cfg(test)]
use crate::text::{LINE_HEIGHT_MULTIPLIER, composer_wrap_line_widths};
use crate::text::{
    markdown_node_has_pixel_runs, measure_markdown_content_height,
    measure_markdown_content_height_cached, measure_raw_content_height, text_content_box_margins,
};

/// Which render-path shaping a flowed child's content should be measured
/// against — mirrors the render path's own dispatch one level up (the
/// `markdown_node_has_pixel_runs` fork in `renderer/text.rs`, which chooses
/// between [`crate::text::TextItem::from_text_markdown_cached`] and
/// [`crate::text::TextItem::from_text_markdown_node`]), so a caller building a
/// [`FlowChild`] from a `TextMarkdownNode` should apply the SAME fork.
#[derive(Clone, Copy, Debug)]
pub(crate) enum FlowContentMode<'a> {
    /// Plain text (e.g. the composer draft). Measured via
    /// [`composer_wrap_line_widths`], the SAME CPU wrapped-line shaper the
    /// composer's own measurement/render paths already share (`WRAPPED_TEXT_WRAP`,
    /// uniform `font_size_px` × [`LINE_HEIGHT_MULTIPLIER`]) — correct for plain
    /// text, but this path does NOT strip markdown syntax or apply per-span
    /// styling, so it must never be used to measure `TextMarkdownNode` content.
    #[cfg(test)]
    PlainText,
    /// Markdown SOURCE with no pixel-bearing `color_runs` — the common case.
    /// Measured via [`crate::text::measure_markdown_content_height`], which
    /// reproduces [`crate::text::TextItem::from_text_markdown_cached`]'s
    /// shaping: `content` is parsed (markdown syntax stripped to `plain_text`,
    /// matching what actually paints), heading `size_scale` spans shape at
    /// their real (taller) per-span metrics, and the token-resolved
    /// `parsed.line_height_multiplier` is used instead of the plain-text
    /// constant. This is the path the intended per-turn transcript
    /// split (hud-uym23) will use for the common (unattributed) case once wired.
    Markdown {
        /// The resolved token set for this content's markdown surface — used to
        /// parse ONLY on the cache-miss fallback, and to derive the margin
        /// line-height (which equals `parsed.line_height_multiplier`).
        tokens: &'a MarkdownTokens,
        /// The commit-time cached parse of `content`, when the render
        /// path's `MarkdownCache` already holds it. `Some` → measured via
        /// [`crate::text::measure_markdown_content_height_cached`] with ZERO
        /// per-frame parse; `None` (cold-frame miss, or a caller with no cache)
        /// → the re-parsing [`crate::text::measure_markdown_content_height`]
        /// fallback. On unchanged content the commit-time prime guarantees `Some`,
        /// so the steady-state per-frame parse cost is zero.
        cached: Option<&'a ParsedMarkdown>,
    },
    /// Markdown SOURCE carrying at least one pixel-bearing `color_run` (e.g.
    /// the per-turn role-attribution span) — measured via
    /// [`crate::text::measure_raw_content_height`], which reproduces
    /// [`crate::text::TextItem::from_text_markdown_node`]'s RAW-content
    /// shaping: `content` is measured EXACTLY as given (markdown syntax NOT
    /// stripped — stripping would shift bytes and invalidate the color run's
    /// pinned offsets), at the node's own `font_family`, using the DEFAULT
    /// line-height multiplier unconditionally (that render constructor has no
    /// access to parsed `MarkdownTokens`). Attributed transcript turns take
    /// this branch rather than [`Self::Markdown`].
    RawWithColorRuns,
}

/// One child's inputs for vertical-flow height measurement.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FlowChild<'a> {
    /// The child's text content (markdown source, as the render path receives it).
    pub(crate) content: &'a str,
    /// The width the child wraps to — its own content box width in px.
    pub(crate) wrap_width: f32,
    /// Font size in px used to shape and measure the child.
    pub(crate) font_size_px: f32,
    /// Font family used to shape and measure the child. Consulted on the
    /// [`FlowContentMode::Markdown`] and [`FlowContentMode::RawWithColorRuns`]
    /// paths — the [`FlowContentMode::PlainText`] path shapes at a fixed
    /// sans-serif family, matching [`composer_wrap_line_widths`]'s existing
    /// behavior.
    pub(crate) font_family: FontFamily,
    /// Total vertical padding (top + bottom) added around the text inside the
    /// child node, matching the render path's `text_margin * 2` so the measured
    /// row height equals the painted row height.
    pub(crate) vertical_padding: f32,
    /// Which render-path shaping to measure `content` against — see
    /// [`FlowContentMode`].
    pub(crate) content_mode: FlowContentMode<'a>,
}

/// Pure geometry: stack `heights` top-to-bottom from `start_y`, inserting `gap`
/// between adjacent items (never before the first or after the last). Returns the
/// resolved top y of each item, in order.
///
/// Defensive clamps keep a malformed input from making the stack run backwards: a
/// negative `gap` is treated as `0.0`, and a negative height contributes `0.0` to
/// the running cursor (its own offset is still emitted). The result length always
/// equals `heights.len()`.
pub(crate) fn stack_offsets(heights: &[f32], gap: f32, start_y: f32) -> Vec<f32> {
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
#[cfg(test)]
pub(crate) fn flow_total_height(heights: &[f32], gap: f32) -> f32 {
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
/// exactly what "measured == painted" covers on each branch.
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
pub(crate) fn measure_child_height(font_system: &mut FontSystem, child: &FlowChild<'_>) -> f32 {
    let wrap_width = child.wrap_width.max(1.0);
    let content_height = match child.content_mode {
        FlowContentMode::Markdown { tokens, cached } => match cached {
            // Cache HIT: measure the commit-time cached parse — the
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
        #[cfg(test)]
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
/// because the render path and this measurement share one margin formula.
///
/// `markdown_tokens` is the caller's resolved token set for the common
/// (non-attributed) markdown case; `flow_child_height` selects which render
/// path a `TextMarkdown` child is measured against by mirroring the SAME
/// `markdown_node_has_pixel_runs` fork the render path itself applies
/// (`renderer/text.rs`) — see [`FlowContentMode`]. This includes
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
/// The compositor-side pre-pass stores these offsets for geometry and text
/// rendering. Children absent from the map retain their explicit vertical origin.
///
/// `markdown_tokens` is the caller's resolved token set for the tile's markdown
/// surface (the SAME set that would be passed to `parse_markdown_subset` /
/// `resolve_portal_tokens` for these nodes) — threaded down to
/// [`flow_child_height`] so a `TextMarkdown` child without pixel-bearing
/// `color_runs` measures against the real tokens rather than the
/// plain-text fallback this pre-pass previously used unconditionally.
///
/// `parsed_by_id` maps a child `SceneId` to the commit-time cached
/// [`ParsedMarkdown`] the render path already produced. On a HIT the
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
                // MarkdownCache already holds its content. Keyed by
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
    use crate::fonts::bundled_font_system;
    use crate::markdown::{PARSE_MARKDOWN_SUBSET_CALLS, parse_markdown_subset};
    use crate::text::{WRAPPED_TEXT_WRAP, markdown_spans_to_styled_runs, styled_run_spans};
    use glyphon::{Attrs, Buffer, Family, Metrics, Shaping, Weight};
    use tze_hud_scene::types::{
        Rect, Rgba, TextAlign, TextColorRun, TextMarkdownNode, TextOverflow,
    };

    #[test]
    fn stack_offsets_cases() {
        type Case = (&'static [f32], f32, f32, &'static [f32], f32);
        let cases: &[Case] = &[
            (&[], 8.0, 0.0, &[], 0.0),
            (&[20.0], 8.0, 5.0, &[5.0], 20.0),
            (&[10.0, 20.0, 5.0], 4.0, 0.0, &[0.0, 14.0, 38.0], 43.0),
            (&[10.0, 10.0], 0.0, 100.0, &[100.0, 110.0], 20.0),
            (&[10.0, -5.0, 7.0], -3.0, 0.0, &[0.0, 10.0, 10.0], 17.0),
        ];
        for &(heights, gap, start, offsets, total) in cases {
            assert_eq!(stack_offsets(heights, gap, start), offsets);
            assert_eq!(flow_total_height(heights, gap), total);
        }
    }

    fn child<'a>(
        content: &'a str,
        wrap_width: f32,
        content_mode: FlowContentMode<'a>,
    ) -> FlowChild<'a> {
        FlowChild {
            content,
            wrap_width,
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            vertical_padding: 0.0,
            content_mode,
        }
    }

    fn markdown(tokens: &MarkdownTokens) -> FlowContentMode<'_> {
        FlowContentMode::Markdown {
            tokens,
            cached: None,
        }
    }

    #[test]
    fn plain_flow_height_has_a_line_floor_and_increases_with_wrap() {
        let mut fs = bundled_font_system();
        let mut child = child("", 200.0, FlowContentMode::PlainText);
        let line = child.font_size_px * LINE_HEIGHT_MULTIPLIER;
        child.vertical_padding = 6.0;
        assert!((measure_child_height(&mut fs, &child) - (line + 6.0)).abs() < 1e-3);
        child.content = "the quick brown fox jumps over the lazy dog again and again";
        child.wrap_width = 6000.0;
        child.vertical_padding = 0.0;
        assert_eq!(measure_child_height(&mut fs, &child), line);
        child.wrap_width = 60.0;
        assert!(measure_child_height(&mut fs, &child) > line);
    }

    /// Assemble the rich buffer independently of the measurement wrapper.
    fn independently_shaped_height(
        fs: &mut FontSystem,
        child: &FlowChild<'_>,
        tokens: &MarkdownTokens,
    ) -> f32 {
        let parsed = parse_markdown_subset(child.content, tokens);
        let runs = markdown_spans_to_styled_runs(&parsed.plain_text, &parsed.spans);
        let size = child.font_size_px;
        let line_height = size * parsed.line_height_multiplier;
        let mut buffer = Buffer::new(fs, Metrics::new(size, line_height));
        buffer.set_size(fs, Some(child.wrap_width), None);
        buffer.set_wrap(fs, WRAPPED_TEXT_WRAP);
        let attrs = Attrs::new().family(Family::SansSerif).weight(Weight(400));
        let spans = styled_run_spans(
            &parsed.plain_text,
            &runs,
            attrs,
            child.font_family,
            size,
            parsed.line_height_multiplier,
        );
        buffer.set_rich_text(fs, spans, attrs, Shaping::Advanced);
        buffer.shape_until_scroll(fs, false);
        let height = buffer.layout_runs().last().unwrap();
        height.line_top + height.line_height + child.vertical_padding
    }

    #[test]
    fn markdown_height_follows_painted_heading_styles_wrapping_and_tokens() {
        let mut fs = bundled_font_system();
        let default = MarkdownTokens::default();
        let tall = MarkdownTokens {
            line_height_multiplier: 2.2,
            ..default.clone()
        };
        for (content, width, tokens, padding, min_height) in [
            (
                "# Big Heading\nbody line",
                400.0,
                &default,
                4.0,
                2.0 * 16.0 * LINE_HEIGHT_MULTIPLIER + 1.0,
            ),
            (
                "one line only",
                400.0,
                &tall,
                0.0,
                16.0 * LINE_HEIGHT_MULTIPLIER + 1.0,
            ),
            (
                "**bold** and *italic* and `inline code` and [a link](https://x.example) and normal text too",
                220.0,
                &default,
                2.0,
                1.5 * 16.0 * LINE_HEIGHT_MULTIPLIER,
            ),
        ] {
            let mut child = child(content, width, markdown(tokens));
            child.vertical_padding = padding;
            let actual = measure_child_height(&mut fs, &child);
            let painted = independently_shaped_height(&mut fs, &child, tokens);
            assert!(
                (actual - painted).abs() < 1e-2,
                "{content}: {actual} vs {painted}"
            );
            assert!(
                actual - padding > min_height,
                "{content}: {actual} <= {min_height} plus {padding}"
            );
        }

        let mut short = child(
            "First paragraph closes the turn.",
            300.0,
            markdown(&default),
        );
        let short_height = measure_child_height(&mut fs, &short);
        short.content = "First paragraph closes the turn.\n\nSecond paragraph.\n\nThird paragraph.";
        assert!(measure_child_height(&mut fs, &short) > short_height);
    }

    #[test]
    fn markdown_height_is_finite_at_degenerate_content_boxes() {
        let mut fs = bundled_font_system();
        let tokens = MarkdownTokens::default();
        for (content, width, size, padding) in [
            ("", 200.0, 16.0, 0.0),
            ("# only a heading", 0.0, 16.0, 0.0),
            ("body", 200.0, 0.0, 0.0),
            ("body", 200.0, 16.0, -10.0),
            ("body", -50.0, 16.0, -5.0),
        ] {
            let mut child = child(content, width, markdown(&tokens));
            child.font_size_px = size;
            child.vertical_padding = padding;
            let height = measure_child_height(&mut fs, &child);
            assert!(height.is_finite() && height >= 0.0, "{child:?}: {height}");
        }
    }

    #[test]
    fn raw_color_run_measurement_preserves_source_family_and_default_line_height() {
        let mut fs = bundled_font_system();
        let tokens = MarkdownTokens::default();
        let content = "**bold** `code` [a link](https://example.com/x) plain";
        let raw = child(content, 260.0, FlowContentMode::RawWithColorRuns);
        let common = child(content, 260.0, markdown(&tokens));
        let plain = child(content, 260.0, FlowContentMode::PlainText);
        let stripped_height = measure_child_height(&mut fs, &common);
        assert!(parse_markdown_subset(content, &tokens).plain_text.len() < content.len());
        assert!(measure_child_height(&mut fs, &raw) > stripped_height);
        assert!(measure_child_height(&mut fs, &plain) > stripped_height);

        let sans = child(
            "the quick brown fox jumps over the lazy dog and then some more words",
            220.0,
            FlowContentMode::RawWithColorRuns,
        );
        let mono = FlowChild {
            font_family: FontFamily::SystemMonospace,
            ..sans
        };
        assert_ne!(
            measure_child_height(&mut fs, &sans),
            measure_child_height(&mut fs, &mono)
        );

        let line = child(
            "one line only, no wrap",
            400.0,
            FlowContentMode::RawWithColorRuns,
        );
        let height = measure_child_height(&mut fs, &line);
        assert!((height - line.font_size_px * tokens.line_height_multiplier).abs() < 1e-2);
    }

    fn text_node(content: &str, y: f32, width: f32) -> Node {
        Node {
            id: SceneId::new(),
            children: Vec::new(),
            layout: NodeLayout::Absolute,
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: content.to_owned(),
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

    fn text(node: &mut Node) -> &mut TextMarkdownNode {
        let NodeData::TextMarkdown(text) = &mut node.data else {
            unreachable!()
        };
        text
    }

    #[test]
    fn flow_child_height_forks_on_pixel_bearing_color_runs() {
        let mut fs = bundled_font_system();
        let tokens = MarkdownTokens {
            line_height_multiplier: 3.0,
            ..Default::default()
        };
        let plain = text_node("**attributed** tool output line", 0.0, 300.0);
        let mut attributed = plain.clone();
        text(&mut attributed).color_runs = Box::from([TextColorRun {
            start_byte: 0,
            end_byte: 4,
            color: Rgba::new(1.0, 0.5, 0.0, 1.0),
        }]);
        assert!(
            flow_child_height(&mut fs, &attributed, &tokens, None)
                < flow_child_height(&mut fs, &plain, &tokens, None),
            "attributed text uses default metrics even when tokens spread the other text"
        );
    }

    #[test]
    fn resolve_tile_flow_offsets_reuses_cached_parse_with_identical_stacking() {
        let mut fs = bundled_font_system();
        let tokens = MarkdownTokens::default();
        let mut first = text_node(
            "**bold** assistant turn with enough text to wrap once",
            0.0,
            200.0,
        );
        let mut second = text_node("# heading\ntool output across two lines here", 0.0, 200.0);
        let end = text_node("fixed absolute control", 50.0, 200.0);
        let mut parent = text_node("", 12.0, 200.0);
        parent.children = vec![first.id, second.id];
        let [first_id, second_id] = [first.id, second.id];
        let cached_first = parse_markdown_subset(&text(&mut first).content, &tokens);
        let cached_second = parse_markdown_subset(&text(&mut second).content, &tokens);
        let cached = HashMap::from([(first_id, &cached_first), (second_id, &cached_second)]);
        let parent_id = parent.id;
        let mut nodes: HashMap<_, _> = [parent, first, second, end]
            .into_iter()
            .map(|n| (n.id, n))
            .collect();
        let gap = 8.0;
        assert!(resolve_tile_flow_offsets(&mut fs, &nodes, gap, &tokens, &cached).is_empty());
        nodes.get_mut(&parent_id).unwrap().layout = NodeLayout::VerticalFlow;

        PARSE_MARKDOWN_SUBSET_CALLS.with(|count| count.set(0));
        let hits = resolve_tile_flow_offsets(&mut fs, &nodes, gap, &tokens, &cached);
        assert_eq!(PARSE_MARKDOWN_SUBSET_CALLS.with(|count| count.get()), 0);
        let misses = resolve_tile_flow_offsets(&mut fs, &nodes, gap, &tokens, &HashMap::new());
        assert_eq!(PARSE_MARKDOWN_SUBSET_CALLS.with(|count| count.get()), 2);
        assert_eq!(hits, misses);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[&first_id], 12.0);
        let first_height =
            flow_child_height(&mut fs, &nodes[&first_id], &tokens, Some(&cached_first));
        assert_eq!(hits[&second_id], 12.0 + first_height + gap);
        assert_eq!(
            first_height,
            flow_child_height(&mut fs, &nodes[&first_id], &tokens, None)
        );
    }

    #[test]
    fn supplied_font_system_changes_only_the_remapped_familys_height() {
        let mut mono = text_node(
            "the quick brown fox jumps over the lazy dog again and again and again \
            and again and again and again and again and again",
            0.0,
            150.0,
        );
        text(&mut mono).font_family = FontFamily::SystemMonospace;
        let sans = text_node(
            "a stable control paragraph whose measurement must never move",
            0.0,
            220.0,
        );
        let tokens = MarkdownTokens::default();
        let mut baseline = bundled_font_system();
        let mut remapped = bundled_font_system();
        remapped.db_mut().set_monospace_family("DejaVu Serif");
        let height = |fonts: &mut FontSystem, node| flow_child_height(fonts, node, &tokens, None);
        assert_ne!(height(&mut baseline, &mono), height(&mut remapped, &mono));
        assert_eq!(height(&mut baseline, &sans), height(&mut remapped, &sans));
    }
}
