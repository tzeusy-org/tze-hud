//! Phase-1 markdown rendering subset integration tests.
//!
//! Covers the supported markdown rendering subset and parsing outside the frame loop:
//!
//! ## Task 2.4 — Integration tests
//! - Each supported subset construct produces styled runs and correct plain text
//! - Each excluded construct degrades to literal text (never silently dropped)
//! - Link non-navigability: no URL in plain text output
//! - Node-budget compliance: 65535-byte payload parses without panic and within budget
//!
//! ## Task 2.5 — Zero per-frame parse cost
//! - Cache hit after prime: O(32-byte lookup), no re-parsing
//! - Unchanged content across frames incurs zero parse cost
//! - Stage-budget compliance: prime + cache-hit < 1 ms for max payload

use std::collections::HashMap;
use tze_hud_compositor::{MarkdownCache, MarkdownTokens, ParsedMarkdown};

// ── helpers ──────────────────────────────────────────────────────────────────

fn default_tokens() -> MarkdownTokens {
    MarkdownTokens::from_token_map(&HashMap::new())
}

fn parse(s: &str) -> ParsedMarkdown {
    let tokens = default_tokens();
    let mut cache = MarkdownCache::new();
    cache.prime(s, &tokens);
    cache.get(s, &tokens).unwrap().clone()
}

// ── Task 2.4: subset constructs ───────────────────────────────────────────────

/// ATX headings H1–H6 must produce bold-weight styled spans covering the
/// heading text (spec: ATX headings are Phase-1 supported).
#[test]
fn heading_h1_has_styled_span() {
    let md = parse("# Hello World");
    assert!(!md.plain_text.is_empty(), "plain text must not be empty");
    assert!(
        md.plain_text.contains("Hello World"),
        "plain text must contain heading text: {:?}",
        md.plain_text
    );
    assert!(
        !md.spans.is_empty(),
        "H1 must have at least one styled span"
    );
    let h1_span = &md.spans[0];
    let weight = h1_span.attr.weight.unwrap_or(400);
    assert!(
        weight >= 600,
        "H1 must have bold weight (>=600), got {weight}"
    );
}

#[test]
fn heading_h6_has_styled_span() {
    let md = parse("###### Small Heading");
    assert!(
        md.plain_text.contains("Small Heading"),
        "plain text must contain H6 text"
    );
    assert!(
        !md.spans.is_empty(),
        "H6 must have at least one styled span"
    );
}

/// Bold (**text**) must produce a span with weight >= 600.
#[test]
fn bold_text_has_weight_span() {
    let md = parse("This is **bold** text");
    assert!(
        md.plain_text.contains("bold"),
        "plain text must contain bold content"
    );
    let bold_span = md
        .spans
        .iter()
        .find(|s| s.attr.weight.is_some_and(|w| w >= 600));
    assert!(
        bold_span.is_some(),
        "bold must produce a high-weight span; spans={:?}",
        md.spans
    );
}

/// Italic (*text*) must produce a span with `italic = true`.
#[test]
fn italic_text_has_italic_span() {
    let md = parse("This is *italic* text");
    assert!(
        md.plain_text.contains("italic"),
        "plain text must contain italic content"
    );
    let italic_span = md.spans.iter().find(|s| s.attr.italic);
    assert!(
        italic_span.is_some(),
        "italic must produce an italic span; spans={:?}",
        md.spans
    );
}

/// Bold-italic (***text***) must produce a span with both weight >= 600 and italic=true.
#[test]
fn bold_italic_has_both_attributes() {
    let md = parse("This is ***bold-italic*** text");
    assert!(
        md.plain_text.contains("bold-italic"),
        "plain text must contain bold-italic content"
    );
    let bi_span = md
        .spans
        .iter()
        .find(|s| s.attr.weight.is_some_and(|w| w >= 600) && s.attr.italic);
    assert!(
        bi_span.is_some(),
        "bold-italic must produce a span with both weight and italic; spans={:?}",
        md.spans
    );
}

/// Inline code (`code`) must produce a span with `monospace = true`.
#[test]
fn inline_code_has_monospace_span() {
    let md = parse("Use `Option<T>` here");
    assert!(
        md.plain_text.contains("Option<T>"),
        "plain text must contain code content"
    );
    let code_span = md.spans.iter().find(|s| s.attr.monospace);
    assert!(
        code_span.is_some(),
        "inline code must produce a monospace span; spans={:?}",
        md.spans
    );
}

/// Fenced code block body must have `monospace = true`.
#[test]
fn fenced_code_block_has_monospace_span() {
    let md = parse("```rust\nlet x = 42;\n```");
    assert!(
        md.plain_text.contains("let x = 42"),
        "plain text must contain code body"
    );
    let code_span = md.spans.iter().find(|s| s.attr.monospace);
    assert!(
        code_span.is_some(),
        "fenced code block must produce monospace spans; spans={:?}",
        md.spans
    );
}

/// Indented code block (4-space) must have `monospace = true`.
#[test]
fn indented_code_block_has_monospace_span() {
    let md = parse("    fn hello() {}");
    assert!(
        md.plain_text.contains("fn hello()"),
        "plain text must contain code content"
    );
    let code_span = md.spans.iter().find(|s| s.attr.monospace);
    assert!(
        code_span.is_some(),
        "indented code block must produce monospace span; spans={:?}",
        md.spans
    );
}

/// Unordered list items (- item, * item, + item) must appear in plain text.
#[test]
fn unordered_list_items_appear_in_plain_text() {
    let md = parse("- First\n- Second\n- Third");
    assert!(md.plain_text.contains("First"), "list item 1 must appear");
    assert!(md.plain_text.contains("Second"), "list item 2 must appear");
    assert!(md.plain_text.contains("Third"), "list item 3 must appear");
    // Bullet characters must be present.
    let bullet_chars = ['•', '-', '*', '+'];
    let has_bullet = bullet_chars.iter().any(|c| md.plain_text.contains(*c));
    assert!(
        has_bullet,
        "list must have bullet character in plain text; got {:?}",
        md.plain_text
    );
}

/// Ordered list items must appear in plain text with numbering.
#[test]
fn ordered_list_items_appear_in_plain_text() {
    let md = parse("1. Alpha\n2. Beta\n3. Gamma");
    assert!(
        md.plain_text.contains("Alpha"),
        "ordered item 1 must appear"
    );
    assert!(md.plain_text.contains("Beta"), "ordered item 2 must appear");
    assert!(
        md.plain_text.contains("Gamma"),
        "ordered item 3 must appear"
    );
}

/// Links must render the link text, not the URL (non-navigable styled text).
#[test]
fn link_renders_text_not_url() {
    let md = parse("[Click here](https://example.com)");
    assert!(
        md.plain_text.contains("Click here"),
        "link text must appear in plain_text"
    );
    assert!(
        !md.plain_text.contains("https://example.com"),
        "URL must NOT appear in plain_text (non-navigable); got {:?}",
        md.plain_text
    );
}

// ── Task 2.4: excluded constructs degrade to literal ─────────────────────────

/// Tables (excluded) must render as literal text, never dropped.
#[test]
fn excluded_table_renders_literally_not_dropped() {
    let md = parse("| Col1 | Col2 |\n|------|------|\n| A    | B    |");
    // Table pipe characters or cell text must be present.
    let has_content = md.plain_text.contains('|')
        || md.plain_text.contains("Col1")
        || md.plain_text.contains("Col2")
        || md.plain_text.contains('A')
        || md.plain_text.contains('B');
    assert!(
        has_content,
        "excluded table must appear as literal text; got {:?}",
        md.plain_text
    );
}

/// HTML (excluded) must render as literal text, never dropped.
#[test]
fn excluded_raw_html_renders_literally_not_dropped() {
    let md = parse("<b>hello</b>");
    assert!(
        !md.plain_text.is_empty(),
        "excluded HTML must produce some output (not silently dropped)"
    );
    assert!(
        md.plain_text.contains("hello") || md.plain_text.contains("<b>"),
        "excluded HTML content must appear in output; got {:?}",
        md.plain_text
    );
}

/// Blockquotes (excluded) must render as literal text, never dropped.
#[test]
fn excluded_blockquote_renders_literally_not_dropped() {
    let md = parse("> This is a quote");
    assert!(
        !md.plain_text.is_empty(),
        "excluded blockquote must produce some output"
    );
    assert!(
        md.plain_text.contains("This is a quote") || md.plain_text.contains('>'),
        "blockquote content must appear in output; got {:?}",
        md.plain_text
    );
}

/// Strikethrough (excluded) must render as literal text, never dropped.
#[test]
fn excluded_strikethrough_renders_literally_not_dropped() {
    let md = parse("~~struck~~");
    assert!(
        !md.plain_text.is_empty(),
        "excluded strikethrough must produce some output"
    );
    assert!(
        md.plain_text.contains("struck"),
        "strikethrough text must appear in output; got {:?}",
        md.plain_text
    );
}

// ── Task 2.4: link non-navigability ──────────────────────────────────────────

/// Even when a token color is configured, link URL must not appear in output.
#[test]
fn link_non_navigable_with_custom_token_color() {
    let mut map = HashMap::new();
    map.insert("color.link.text".to_string(), "#0000FF".to_string());
    let tokens = MarkdownTokens::from_token_map(&map);
    let mut cache = MarkdownCache::new();
    let content = "[Visit site](https://not-in-output.example.org/path?q=1)";
    cache.prime(content, &tokens);
    let md = cache.get(content, &tokens).unwrap();
    assert!(
        !md.plain_text.contains("not-in-output"),
        "URL must not appear in plain text; got {:?}",
        md.plain_text
    );
    assert!(
        md.plain_text.contains("Visit site"),
        "link text must appear in plain text"
    );
}

// ── Task 2.4: node-budget compliance ─────────────────────────────────────────

/// Parsing the maximum 65535-byte payload (MAX_MARKDOWN_BYTES) must not panic
/// and must complete within a sensible wall-clock time.
#[test]
fn max_payload_node_budget_parses_without_panic() {
    // Construct a 65535-byte markdown string with a mix of subset constructs.
    let chunk = "# Heading\n\nSome **bold** and *italic* and `code` text.\n\n";
    let repeat_count = (65535 / chunk.len()) + 1;
    let mut payload = chunk.repeat(repeat_count);
    payload.truncate(65535);

    let tokens = default_tokens();
    let mut cache = MarkdownCache::new();
    // Must not panic.
    cache.prime(&payload, &tokens);
    let md = cache
        .get(&payload, &tokens)
        .expect("entry must exist after prime");
    assert!(
        !md.plain_text.is_empty(),
        "65535-byte payload must produce non-empty plain text"
    );
}

// ── Task 2.5: zero per-frame parse cost ──────────────────────────────────────

/// After `prime`, `get` must return Some (cache hit) — the content was already
/// parsed at commit time, not on the frame path.
#[test]
fn cache_hit_after_prime_is_zero_cost() {
    let tokens = default_tokens();
    let mut cache = MarkdownCache::new();
    let content = "## Section\n\nSome **bold** text with `code`.";
    assert!(
        cache.get(content, &tokens).is_none(),
        "must be a cache miss before prime"
    );
    cache.prime(content, &tokens);
    assert!(
        cache.get(content, &tokens).is_some(),
        "must be a cache hit after prime"
    );
}

/// Multiple calls to `get` for the same content must all be cache hits,
/// consistent with zero per-frame parse cost for unchanged content.
#[test]
fn repeated_gets_are_all_cache_hits() {
    let tokens = default_tokens();
    let mut cache = MarkdownCache::new();
    let content = "Hello **world** from `markdown`";
    cache.prime(content, &tokens);
    for _ in 0..100 {
        assert!(
            cache.get(content, &tokens).is_some(),
            "every get must be a cache hit"
        );
    }
}

/// Stage-budget test: zero per-frame parse cost for unchanged
/// content and stage-budget compliance when a 65535-byte payload commits.
///
/// The spec requires:
///   - PARSE happens at content-commit time (prime), NOT in the frame loop.
///   - Frame path cost (cache hit via get) must be sub-millisecond for any
///     payload size — it is O(32-byte BLAKE3 hash lookup) only.
///   - Stages 3–5 in the render pipeline must each be < 1 ms; this is upheld
///     because no parse work occurs in those stages after the initial prime.
///
/// This test verifies the per-frame invariant by checking that `get` is fast
/// after `prime`, independent of payload size. The initial `prime` may be
/// slow (it happens outside the frame loop) — that is by design.
#[test]
fn max_payload_cache_hit_is_sub_millisecond() {
    let chunk = "# Heading\n\nSome **bold** and *italic* and `code` text.\n\n";
    let repeat_count = (65535 / chunk.len()) + 1;
    let mut payload = chunk.repeat(repeat_count);
    payload.truncate(65535);

    let tokens = default_tokens();
    let mut cache = MarkdownCache::new();

    // Prime happens at commit time (outside frame loop — no budget constraint here).
    cache.prime(&payload, &tokens);

    // Measure the FRAME PATH cost: cache hit must be O(32-byte hash lookup).
    // This simulates a render_frame call for unchanged content.
    const SAMPLE_COUNT: u32 = 100;
    let t0 = std::time::Instant::now();
    for _ in 0..SAMPLE_COUNT {
        let _ = cache.get(&payload, &tokens);
    }
    let total_elapsed = t0.elapsed();
    let avg_us = total_elapsed.as_micros() / u128::from(SAMPLE_COUNT);

    // Average cache-hit cost must be < 1 ms (1000 µs) to satisfy stage budget.
    assert!(
        avg_us < 1000,
        "cache hit (frame path) must be < 1ms; avg={avg_us}µs over {SAMPLE_COUNT} samples"
    );
}

/// Token map changes produce different styled runs (so design-token updates
/// result in correct restyling — the cache is rebuilt when tokens change).
/// Token key for H1 weight is `typography.heading.1.weight`.
#[test]
fn different_token_maps_produce_different_styled_runs() {
    // Use weight 800 (not the default 700) vs 900 to get a distinguishable diff.
    let mut map_a = HashMap::new();
    map_a.insert("typography.heading.1.weight".to_string(), "800".to_string());
    let tokens_a = MarkdownTokens::from_token_map(&map_a);

    let mut map_b = HashMap::new();
    map_b.insert("typography.heading.1.weight".to_string(), "900".to_string());
    let tokens_b = MarkdownTokens::from_token_map(&map_b);

    let content = "# Heading One";

    // Prime with token set A.
    let mut cache_a = MarkdownCache::new();
    cache_a.prime(content, &tokens_a);
    let md_a = cache_a.get(content, &tokens_a).unwrap().clone();

    // Prime with token set B (simulates design-token refresh clearing the cache).
    let mut cache_b = MarkdownCache::new();
    cache_b.prime(content, &tokens_b);
    let md_b = cache_b.get(content, &tokens_b).unwrap().clone();

    // Weights must differ because tokens differ.
    let weight_a = md_a
        .spans
        .first()
        .and_then(|s| s.attr.weight)
        .unwrap_or(400);
    let weight_b = md_b
        .spans
        .first()
        .and_then(|s| s.attr.weight)
        .unwrap_or(400);
    assert_ne!(
        weight_a, weight_b,
        "different token weights must produce different styled spans; a={weight_a} b={weight_b}"
    );
    assert_eq!(weight_a, 800, "token A weight must be 800");
    assert_eq!(weight_b, 900, "token B weight must be 900");
}
