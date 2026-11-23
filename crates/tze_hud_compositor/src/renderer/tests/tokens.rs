use super::*;

#[test]
fn composer_tokens_default_to_readable_and_distinct_colors() {
    let t = resolve_composer_overlay_tokens(&HashMap::new());
    assert!(t.font_size_px.is_finite() && t.font_size_px >= 16.0);
    assert!(t.bg_a > 0.0 && t.at_capacity_a > 0.0);
    assert!((t.at_capacity_r - t.bg_r).abs() > 0.1);
    for (name, channels) in [
        ("background", [t.bg_r, t.bg_g, t.bg_b, t.bg_a]),
        ("text", [t.text_r, t.text_g, t.text_b, t.text_a]),
        (
            "at capacity",
            [
                t.at_capacity_r,
                t.at_capacity_g,
                t.at_capacity_b,
                t.at_capacity_a,
            ],
        ),
    ] {
        assert!(
            channels.iter().all(|c| (0.0..=1.0).contains(c)),
            "{name}: {channels:?}"
        );
    }

    let srgb = |v: f32| (super::token_colors::linear_to_srgb(v) * 255.0 + 0.5) as u8;
    let text = [
        srgb(t.text_r),
        srgb(t.text_g),
        srgb(t.text_b),
        (t.text_a * 255.0 + 0.5) as u8,
    ];
    assert_eq!(
        t.caret_color, text,
        "caret follows the text color by default"
    );
    let [red, green, blue, alpha] = t.selection_bg;
    assert!(alpha > 0 && blue > red && blue > green);
}

#[test]
fn composer_caret_selection_and_capacity_have_independent_overrides() {
    let default = resolve_composer_overlay_tokens(&HashMap::new());
    let resolve = |key: &str, value: &str| {
        resolve_composer_overlay_tokens(&HashMap::from([(key.into(), value.into())]))
    };
    let capacity = resolve("portal.composer.at_capacity_color", "#FF0000");
    assert!(
        capacity.at_capacity_r > 0.9
            && capacity.at_capacity_g < 0.1
            && capacity.at_capacity_b < 0.1
    );
    assert!((capacity.at_capacity_r - default.at_capacity_r).abs() > 0.1);
    assert_eq!(capacity.caret_color, default.caret_color);
    assert_eq!(capacity.selection_bg, default.selection_bg);

    let caret = resolve("portal.composer.caret_color", "#00FF00FF");
    assert_eq!(caret.caret_color, [0, 255, 0, 255]);
    assert_ne!(caret.caret_color, default.caret_color);
    assert_eq!(caret.selection_bg, default.selection_bg);

    let selection = resolve("portal.composer.selection_color", "#FF0000FF");
    assert_eq!(selection.selection_bg, [255, 0, 0, 255]);
    assert_eq!(selection.caret_color, default.caret_color);
}

#[test]
fn tile_background_roles_follow_their_own_tokens_and_fallbacks() {
    let empty = HashMap::new();
    for (key, fallback, override_hex, expected_rgb) in [
        (
            "color.tile.background.text_markdown",
            TILE_BG_TEXT_MARKDOWN,
            "#00FFFF",
            [0.0, 1.0, 1.0],
        ),
        (
            "color.tile.background.static_image",
            TILE_BG_STATIC_IMAGE,
            "#FF0000",
            [1.0, 0.0, 0.0],
        ),
        (
            "color.tile.background.default",
            TILE_BG_DEFAULT,
            "#00FF00",
            [0.0, 1.0, 0.0],
        ),
    ] {
        assert_eq!(
            resolve_tile_bg_token(&empty, key, fallback),
            fallback,
            "{key}"
        );
        let token = HashMap::from([(key.into(), override_hex.into())]);
        let actual = resolve_tile_bg_token(&token, key, fallback);
        for (actual, expected) in [actual.r, actual.g, actual.b].into_iter().zip(expected_rgb) {
            assert!(
                (actual - expected).abs() < 0.01,
                "{key} color: {actual} vs {expected}"
            );
        }
    }

    let markdown = resolve_tile_bg_token(
        &empty,
        "color.tile.background.text_markdown",
        TILE_BG_TEXT_MARKDOWN,
    );
    for (actual, expected) in [markdown.r, markdown.g, markdown.b]
        .into_iter()
        .zip([0.15, 0.15, 0.25])
    {
        assert!((actual - expected).abs() < 1e-4);
    }
}
