use super::*;

/// `resolve_composer_overlay_tokens` must return valid, non-degenerate token
/// values for an empty token map (all defaults applied).
///
/// This is a CPU-only smoke test — no GPU required.
#[test]
fn composer_overlay_tokens_defaults_are_valid() {
    use std::collections::HashMap;

    // Empty token map → all defaults kick in.
    let tokens: std::collections::HashMap<String, String> = HashMap::new();
    let t = resolve_composer_overlay_tokens(&tokens);

    // Font size must be finite and positive.
    assert!(
        t.font_size_px.is_finite() && t.font_size_px > 0.0,
        "font_size_px must be positive finite, got {}",
        t.font_size_px
    );

    // Background alpha must be > 0 so the overlay is actually visible.
    assert!(
        t.bg_a > 0.0,
        "default background alpha must be > 0 (overlay must be visible)"
    );

    // All color channels must be in [0, 1].
    for (name, val) in [
        ("bg_r", t.bg_r),
        ("bg_g", t.bg_g),
        ("bg_b", t.bg_b),
        ("bg_a", t.bg_a),
        ("text_r", t.text_r),
        ("text_g", t.text_g),
        ("text_b", t.text_b),
        ("text_a", t.text_a),
        ("at_capacity_r", t.at_capacity_r),
        ("at_capacity_g", t.at_capacity_g),
        ("at_capacity_b", t.at_capacity_b),
        ("at_capacity_a", t.at_capacity_a),
    ] {
        assert!(
            (0.0..=1.0).contains(&val),
            "token {name} must be in [0, 1], got {val}"
        );
    }
}

#[test]
fn composer_overlay_default_font_size_matches_readable_portal_default() {
    use std::collections::HashMap;

    let tokens: HashMap<String, String> = HashMap::new();
    let resolved = resolve_composer_overlay_tokens(&tokens);

    assert!(
        resolved.font_size_px >= 16.0,
        "focused composer overlay default font must match the readable portal composer default; got {}px",
        resolved.font_size_px
    );
}

/// The at-capacity color token must be distinct from the background color
/// so it provides a visible signal when the composer draft reaches its byte cap.
///
/// Verifies two things:
/// 1. The default at-capacity color (muted amber `#B87333`) has a non-zero red
///    channel while the background (`#0F1418`) has a near-zero red channel —
///    they are visually distinguishable.
/// 2. Injecting an override via the token map propagates through
///    `resolve_composer_overlay_tokens` so the compositor render path is driven
///    entirely by the token, with no hardcoded color values.
///
/// CPU-only — no GPU required (hud-2axdq acceptance criterion).
#[test]
fn composer_at_capacity_token_is_distinct_from_background_and_propagates_override() {
    use std::collections::HashMap;

    // Default case: at-capacity color must differ from background.
    let empty: HashMap<String, String> = HashMap::new();
    let t = resolve_composer_overlay_tokens(&empty);

    // At-capacity alpha must be non-zero so the indicator is actually visible.
    assert!(
        t.at_capacity_a > 0.0,
        "default at_capacity_a must be > 0 (indicator must be visible)"
    );
    // The default at-capacity color (amber #B87333) has high red channel;
    // the default background (#0F1418) has very low red channel. They must differ.
    assert!(
        (t.at_capacity_r - t.bg_r).abs() > 0.1,
        "at_capacity_r ({}) must differ meaningfully from bg_r ({}) so the \
             at-capacity indicator is visually distinct from the composer background",
        t.at_capacity_r,
        t.bg_r,
    );

    // Override case: injecting a token value must propagate to the struct.
    let mut overrides: HashMap<String, String> = HashMap::new();
    overrides.insert(
        "portal.composer.at_capacity_color".to_string(),
        "#FF0000".to_string(), // pure red sentinel
    );
    let t_override = resolve_composer_overlay_tokens(&overrides);
    // Red channel must be ~1.0 (pure red), not the default amber.
    assert!(
        t_override.at_capacity_r > 0.9,
        "overridden at_capacity_r must be ~1.0 (pure red sentinel), got {}",
        t_override.at_capacity_r
    );
    assert!(
        t_override.at_capacity_g < 0.1,
        "overridden at_capacity_g must be ~0.0 (pure red sentinel), got {}",
        t_override.at_capacity_g
    );
    assert!(
        t_override.at_capacity_b < 0.1,
        "overridden at_capacity_b must be ~0.0 (pure red sentinel), got {}",
        t_override.at_capacity_b
    );
    // Baseline must differ from override (amber vs red).
    assert!(
        (t.at_capacity_r - t_override.at_capacity_r).abs() > 0.1,
        "default and overridden at_capacity_r must differ (amber vs red)"
    );
}

// ── Composer caret-color tokenization tests [hud-khfgx] ──────────────────

/// The caret color defaults to the composer text color, so tokenizing the caret
/// (vd-caret-selection-placeholder-not-tokenized) is a no-visual-regression change
/// for the default profile: with no `portal.composer.caret_color` token, the
/// resolved caret color equals the resolved composer text color (both in sRGB u8).
///
/// CPU-only — no GPU required.
#[test]
fn composer_caret_color_defaults_to_text_color() {
    use super::token_colors::linear_to_srgb;
    use std::collections::HashMap;

    let empty: HashMap<String, String> = HashMap::new();
    let t = resolve_composer_overlay_tokens(&empty);

    let to_srgb_u8 = |v: f32| (linear_to_srgb(v.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
    let expected = [
        to_srgb_u8(t.text_r),
        to_srgb_u8(t.text_g),
        to_srgb_u8(t.text_b),
        (t.text_a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
    ];
    assert_eq!(
        t.caret_color, expected,
        "default caret color must equal the composer text color (no visual regression)"
    );
}

/// A `portal.composer.caret_color` override propagates to
/// `ComposerOverlayTokens::caret_color` so the caret can be accented independently
/// of the composer text color.
///
/// CPU-only — no GPU required.
#[test]
fn composer_caret_color_token_override_propagates() {
    use std::collections::HashMap;

    let mut overrides: HashMap<String, String> = HashMap::new();
    // Pure-green sentinel, distinct from the default near-white text color.
    overrides.insert(
        "portal.composer.caret_color".to_string(),
        "#00FF00FF".to_string(),
    );
    let t = resolve_composer_overlay_tokens(&overrides);

    assert_eq!(
        t.caret_color[0], 0x00,
        "overridden caret red channel must be 0x00, got {:?}",
        t.caret_color
    );
    assert_eq!(
        t.caret_color[1], 0xFF,
        "overridden caret green channel must be 0xFF, got {:?}",
        t.caret_color
    );
    assert_eq!(
        t.caret_color[3], 0xFF,
        "overridden caret alpha must be 0xFF, got {:?}",
        t.caret_color
    );
    // The override must differ from the default (which tracks the text color).
    let default = resolve_composer_overlay_tokens(&HashMap::new());
    assert_ne!(
        t.caret_color, default.caret_color,
        "caret color override must differ from the default text-colored caret"
    );
}

// ── Composer selection-range rendering tests [hud-bq0gl.9] ───────────────

/// The default `selection_bg` token must have a non-zero alpha so selection
/// highlights are actually visible when no `portal.composer.selection_color`
/// token is configured.
///
/// CPU-only — no GPU required.
#[test]
fn composer_selection_bg_default_is_visible() {
    use std::collections::HashMap;

    let empty: HashMap<String, String> = HashMap::new();
    let t = resolve_composer_overlay_tokens(&empty);

    // Alpha is the 4th element; must be > 0 for the highlight to show.
    assert!(
        t.selection_bg[3] > 0,
        "default selection_bg alpha must be > 0 so selection highlights are visible, \
         got {:?}",
        t.selection_bg,
    );
    // Blue channel should dominate the default blue-tint selection color.
    assert!(
        t.selection_bg[2] > t.selection_bg[0] && t.selection_bg[2] > t.selection_bg[1],
        "default selection_bg should be a blue-dominant color (#3A7BD5), got {:?}",
        t.selection_bg,
    );
}

/// `portal.composer.selection_color` token override must propagate to
/// `ComposerOverlayTokens::selection_bg` correctly.
///
/// CPU-only — no GPU required.
#[test]
fn composer_selection_bg_token_override_propagates() {
    use std::collections::HashMap;

    let mut overrides: HashMap<String, String> = HashMap::new();
    // Pure red sentinel in sRGB hex.
    overrides.insert(
        "portal.composer.selection_color".to_string(),
        "#FF0000FF".to_string(),
    );
    let t = resolve_composer_overlay_tokens(&overrides);

    assert_eq!(
        t.selection_bg[0], 0xFF,
        "overridden selection_bg red channel must be 0xFF, got {:?}",
        t.selection_bg
    );
    assert_eq!(
        t.selection_bg[1], 0x00,
        "overridden selection_bg green channel must be 0x00, got {:?}",
        t.selection_bg
    );
    assert_eq!(
        t.selection_bg[2], 0x00,
        "overridden selection_bg blue channel must be 0x00, got {:?}",
        t.selection_bg
    );
    assert_eq!(
        t.selection_bg[3], 0xFF,
        "overridden selection_bg alpha channel must be 0xFF, got {:?}",
        t.selection_bg
    );
}

// Note (hud-hxhnt): the byte-offset-mapping test that used to live here
// (`composer_selection_display_byte_offsets`) verified the +3-byte caret-glyph
// shift arithmetic against the (now removed) `composer_display_text` glyph
// insertion helper. That accounting no longer exists — the caret is a
// chrome-layer quad, and the selection styled run uses RAW byte offsets — so
// the test was replaced by `composer_selection_styled_run_uses_raw_byte_offsets`
// above, which exercises the same five (cursor, anchor) cases through the real
// production `collect_composer_text_item` path instead of reimplementing the
// arithmetic against a free-standing helper.

// ── Tile background color token tests [hud-9wljr.10] ─────────────────────

/// `resolve_tile_bg_token` returns the fallback when the token map is empty.
///
/// CPU-only — no GPU required.
#[test]
fn resolve_tile_bg_token_returns_fallback_on_absent_token() {
    let token_map: HashMap<String, String> = HashMap::new();

    let c = resolve_tile_bg_token(
        &token_map,
        "color.tile.background.text_markdown",
        TILE_BG_TEXT_MARKDOWN,
    );
    assert!(
        (c.r - TILE_BG_TEXT_MARKDOWN.r).abs() < f32::EPSILON
            && (c.g - TILE_BG_TEXT_MARKDOWN.g).abs() < f32::EPSILON
            && (c.b - TILE_BG_TEXT_MARKDOWN.b).abs() < f32::EPSILON,
        "absent text_markdown token must fall back to TILE_BG_TEXT_MARKDOWN, got {c:?}"
    );

    let c = resolve_tile_bg_token(
        &token_map,
        "color.tile.background.static_image",
        TILE_BG_STATIC_IMAGE,
    );
    assert!(
        (c.r - TILE_BG_STATIC_IMAGE.r).abs() < f32::EPSILON
            && (c.g - TILE_BG_STATIC_IMAGE.g).abs() < f32::EPSILON
            && (c.b - TILE_BG_STATIC_IMAGE.b).abs() < f32::EPSILON,
        "absent static_image token must fall back to TILE_BG_STATIC_IMAGE, got {c:?}"
    );

    let c = resolve_tile_bg_token(&token_map, "color.tile.background.default", TILE_BG_DEFAULT);
    assert!(
        (c.r - TILE_BG_DEFAULT.r).abs() < f32::EPSILON
            && (c.g - TILE_BG_DEFAULT.g).abs() < f32::EPSILON
            && (c.b - TILE_BG_DEFAULT.b).abs() < f32::EPSILON,
        "absent default token must fall back to TILE_BG_DEFAULT, got {c:?}"
    );
}

/// Token override: `color.tile.background.text_markdown` overrides the fallback.
///
/// Uses pure cyan (#00FFFF) — clearly distinct from the default blue-gray.
/// CPU-only — no GPU required.
#[test]
fn resolve_tile_bg_token_text_markdown_override() {
    let mut token_map: HashMap<String, String> = HashMap::new();
    token_map.insert(
        "color.tile.background.text_markdown".to_string(),
        "#00FFFF".to_string(),
    );

    let c = resolve_tile_bg_token(
        &token_map,
        "color.tile.background.text_markdown",
        TILE_BG_TEXT_MARKDOWN,
    );
    // #00FFFF sRGB → linear: R=0.0, G≈1.0, B≈1.0
    assert!(
        c.r < 0.01,
        "overridden text_markdown R should be ~0.0 (cyan), got {}",
        c.r
    );
    assert!(
        c.g > 0.9,
        "overridden text_markdown G should be ~1.0 (cyan), got {}",
        c.g
    );
    assert!(
        c.b > 0.9,
        "overridden text_markdown B should be ~1.0 (cyan), got {}",
        c.b
    );
}

/// Token override: `color.tile.background.static_image` overrides the fallback.
///
/// Uses pure red (#FF0000) — clearly distinct from the default near-black.
/// CPU-only — no GPU required.
#[test]
fn resolve_tile_bg_token_static_image_override() {
    let mut token_map: HashMap<String, String> = HashMap::new();
    token_map.insert(
        "color.tile.background.static_image".to_string(),
        "#FF0000".to_string(),
    );

    let c = resolve_tile_bg_token(
        &token_map,
        "color.tile.background.static_image",
        TILE_BG_STATIC_IMAGE,
    );
    // #FF0000 sRGB → linear: R≈1.0, G=0.0, B=0.0
    assert!(
        c.r > 0.9,
        "overridden static_image R should be ~1.0 (red), got {}",
        c.r
    );
    assert!(
        c.g < 0.01,
        "overridden static_image G should be ~0.0 (red), got {}",
        c.g
    );
    assert!(
        c.b < 0.01,
        "overridden static_image B should be ~0.0 (red), got {}",
        c.b
    );
}

/// Token override: `color.tile.background.default` overrides the fallback.
///
/// Uses pure green (#00FF00) — clearly distinct from the default blue-dark.
/// CPU-only — no GPU required.
#[test]
fn resolve_tile_bg_token_default_override() {
    let mut token_map: HashMap<String, String> = HashMap::new();
    token_map.insert(
        "color.tile.background.default".to_string(),
        "#00FF00".to_string(),
    );

    let c = resolve_tile_bg_token(&token_map, "color.tile.background.default", TILE_BG_DEFAULT);
    // #00FF00 sRGB → linear: R=0.0, G≈1.0, B=0.0
    assert!(
        c.r < 0.01,
        "overridden default R should be ~0.0 (green), got {}",
        c.r
    );
    assert!(
        c.g > 0.9,
        "overridden default G should be ~1.0 (green), got {}",
        c.g
    );
    assert!(
        c.b < 0.01,
        "overridden default B should be ~0.0 (green), got {}",
        c.b
    );
}
