//! Token-styled, geometry-only scroll-position indicators for portal panes.
//!
//!
//! > Implement token-styled, geometry-only scroll-position indicators for
//! > overflowing transcript/composer panes, redaction-safe.
//!
//! ## Design
//!
//! The indicator is **geometry-only**: it encodes scroll position (where you are
//! in the content) and size (how much of the content is visible), but it carries
//! **no content** — no transcript text, no draft text, no session information.
//! It is therefore safe to render for any viewer, including restricted viewers
//! operating under redaction policy.
//!
//! The indicator is fully token-driven. Every visual property (color, width,
//! minimum thumb height) comes from `ScrollIndicatorTokens`, which the caller
//! resolves from `PortalPartTokens`. No literal values are permitted in the
//! geometry computation itself; `ScrollIndicatorTokens::default()` exists only
//! as a fallback/test helper and its values must mirror the canonical defaults
//! in `tze_hud_config::portal_tokens`.
//!
//! ## Terminology
//!
//! - **viewport_px** — the visible height of the scrollable pane in pixels.
//! - **content_px** — the total height of the content in pixels.
//! - **scroll_offset_px** — how far the viewport is scrolled from the top.
//! - **thumb** — the visible portion of the indicator track that represents
//!   the viewport window within the content.
//! - **track** — the full height of the indicator, equal to the viewport height.
//!
//! ## Invariants
//!
//! - When `content_px <= viewport_px`, the indicator is not visible (no overflow).
//! - The thumb height is at least `min_thumb_height_px` to remain interactive.
//! - The thumb never extends outside the track bounds.
//! - All geometry is computed in f32 pixels; callers use this for GPU rendering.
//!
//! ## Redaction safety
//!
//! `ScrollIndicatorGeometry` carries only `thumb_y_px`, `thumb_height_px`,
//! `track_height_px`, and `width_px`. There is no transcript excerpt, no line
//! count, and no character index. The indicator reveals only that content is
//! overflowing and approximately where the viewport sits — the same information
//! visible from the scrollbar position in any text editor.

use serde::{Deserialize, Serialize};

// ─── Token-resolved indicator styling ────────────────────────────────────────

/// Token-resolved visual properties for the scroll-position indicator.
///
/// Resolved from `PortalPartTokens` at startup. Passed by value into geometry
/// calculations so the compositor never reads tokens from a map on the hot path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollIndicatorTokens {
    /// Thumb color (RGBA components, each in [0.0, 1.0]).
    pub color_r: f32,
    pub color_g: f32,
    pub color_b: f32,
    pub color_a: f32,
    /// Track (and thumb) width in pixels.
    pub width_px: f32,
    /// Minimum thumb height in pixels (prevents thumb from vanishing on deep content).
    pub min_thumb_height_px: f32,
}

impl Default for ScrollIndicatorTokens {
    fn default() -> Self {
        // Must match `portal_tokens::defaults::SCROLL_INDICATOR_*`
        // in tze_hud_config. No compile-time link; update both sides when
        // changing defaults.
        Self {
            // #4A5568 with full opacity
            color_r: 0x4A as f32 / 255.0,
            color_g: 0x55 as f32 / 255.0,
            color_b: 0x68 as f32 / 255.0,
            color_a: 1.0,
            width_px: 4.0,
            min_thumb_height_px: 24.0,
        }
    }
}

// ─── Scroll indicator geometry ────────────────────────────────────────────────

/// Computed geometry for a scroll-position indicator thumb.
///
/// Returned by [`compute_scroll_indicator`]. All fields are in pixels
/// relative to the top-left of the **pane** (not the display).
///
/// `None` is returned by [`compute_scroll_indicator`] when the pane is not
/// overflowing — callers MUST not render the indicator in that case.
///
/// This struct is **geometry-only**: it carries no content information.
/// It is safe to deliver to any viewer under any redaction policy.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollIndicatorGeometry {
    /// Y offset of the thumb's top edge within the track (0.0 = track top).
    pub thumb_y_px: f32,
    /// Height of the thumb.
    pub thumb_height_px: f32,
    /// Full height of the track (equals the viewport height).
    pub track_height_px: f32,
    /// Track (and thumb) width from tokens.
    pub width_px: f32,
}

// ─── Computation ─────────────────────────────────────────────────────────────

/// Compute scroll-position indicator geometry for a single pane.
///
/// Returns `None` when the pane does not overflow (`content_px <= viewport_px`)
/// or when any dimension is non-positive.
///
/// # Arguments
///
/// * `viewport_px` — visible height of the scrollable pane.
/// * `content_px` — total content height.
/// * `scroll_offset_px` — how far the content has been scrolled from the top.
///   Clamped to `[0, content_px - viewport_px]` internally.
/// * `tokens` — resolved indicator styling from the portal token set.
///
/// # Geometry
///
/// ```text
/// track  (height = viewport_px)
/// ┌────┐ ← y=0
/// │    │
/// │####│ ← thumb_y_px
/// │####│
/// │####│ ← thumb_y_px + thumb_height_px
/// │    │
/// └────┘ ← y=viewport_px
/// ```
///
/// Thumb height: `viewport_px / content_px * viewport_px`, clamped to
/// `[min_thumb_height_px, viewport_px]`.
///
/// Thumb y: `scroll_fraction * (track_height - thumb_height)`, where
/// `scroll_fraction = scroll_offset_px / (content_px - viewport_px)`.
pub fn compute_scroll_indicator(
    viewport_px: f32,
    content_px: f32,
    scroll_offset_px: f32,
    tokens: &ScrollIndicatorTokens,
) -> Option<ScrollIndicatorGeometry> {
    // Reject non-finite inputs (NaN, ±inf) before any comparison: NaN
    // comparisons always return false, so `viewport_px <= 0.0` would silently
    // pass for NaN and produce a NaN geometry result.
    if !viewport_px.is_finite() || !content_px.is_finite() || !scroll_offset_px.is_finite() {
        return None;
    }
    if viewport_px <= 0.0 || content_px <= 0.0 {
        return None;
    }
    if content_px <= viewport_px {
        // No overflow — indicator not shown.
        return None;
    }

    let overflow = content_px - viewport_px; // always > 0

    // Clamp scroll offset to valid range.
    let offset = scroll_offset_px.clamp(0.0, overflow);

    // Thumb height proportional to the visible fraction of content.
    let thumb_h = (viewport_px / content_px * viewport_px)
        .max(tokens.min_thumb_height_px)
        .min(viewport_px);

    // Thumb y: maps scroll fraction to position in track.
    let travel = viewport_px - thumb_h; // available travel in the track
    let scroll_frac = if overflow > 0.0 {
        offset / overflow
    } else {
        0.0
    };
    let thumb_y = (scroll_frac * travel).clamp(0.0, travel);

    Some(ScrollIndicatorGeometry {
        thumb_y_px: thumb_y,
        thumb_height_px: thumb_h,
        track_height_px: viewport_px,
        width_px: tokens.width_px,
    })
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_tokens() -> ScrollIndicatorTokens {
        ScrollIndicatorTokens::default() // width=4, min_thumb=24
    }

    // ─── Basic geometry ────────────────────────────────────────────────────

    #[test]
    fn no_indicator_when_content_fits_in_viewport() {
        let tokens = default_tokens();
        let result = compute_scroll_indicator(400.0, 400.0, 0.0, &tokens);
        assert!(
            result.is_none(),
            "indicator must not appear when content fits in viewport"
        );
    }

    #[test]
    fn no_indicator_when_content_shorter_than_viewport() {
        let tokens = default_tokens();
        let result = compute_scroll_indicator(400.0, 200.0, 0.0, &tokens);
        assert!(
            result.is_none(),
            "indicator must not appear when content is shorter than viewport"
        );
    }

    #[test]
    fn indicator_present_when_overflowing() {
        let tokens = default_tokens();
        let result = compute_scroll_indicator(400.0, 1200.0, 0.0, &tokens);
        assert!(
            result.is_some(),
            "indicator must appear when content overflows"
        );
    }

    // ─── Thumb at scroll extremes ──────────────────────────────────────────

    #[test]
    fn thumb_at_top_when_scroll_offset_zero() {
        let tokens = default_tokens();
        let geom = compute_scroll_indicator(400.0, 1200.0, 0.0, &tokens).unwrap();
        assert!(
            geom.thumb_y_px.abs() < 1.0,
            "thumb must be at y=0 when scroll_offset=0"
        );
    }

    #[test]
    fn thumb_at_bottom_when_scrolled_to_end() {
        let tokens = default_tokens();
        let viewport = 400.0;
        let content = 1200.0;
        let max_scroll = content - viewport;
        let geom = compute_scroll_indicator(viewport, content, max_scroll, &tokens).unwrap();
        let track_bottom = geom.track_height_px;
        let thumb_bottom = geom.thumb_y_px + geom.thumb_height_px;
        assert!(
            (thumb_bottom - track_bottom).abs() < 1.0,
            "thumb bottom must reach track bottom when scrolled to end"
        );
    }

    #[test]
    fn thumb_at_midpoint_when_half_scrolled() {
        let tokens = default_tokens();
        let viewport = 400.0;
        let content = 800.0;
        let half_scroll = (content - viewport) / 2.0; // = 200.0
        let geom = compute_scroll_indicator(viewport, content, half_scroll, &tokens).unwrap();

        // thumb should be at roughly half of travel
        let thumb_h = geom.thumb_height_px;
        let travel = viewport - thumb_h;
        let expected_y = travel * 0.5;
        assert!(
            (geom.thumb_y_px - expected_y).abs() < 1.0,
            "thumb y must be at half travel when half-scrolled"
        );
    }

    // ─── Minimum thumb height ──────────────────────────────────────────────

    #[test]
    fn thumb_height_at_least_min_thumb_height_px() {
        let tokens = default_tokens(); // min_thumb = 24
        // Very deep content — would make thumb tiny without clamping
        let geom = compute_scroll_indicator(400.0, 400_000.0, 0.0, &tokens).unwrap();
        assert!(
            geom.thumb_height_px >= tokens.min_thumb_height_px,
            "thumb height must be at least min_thumb_height_px for very deep content"
        );
    }

    #[test]
    fn thumb_height_never_exceeds_track() {
        let tokens = default_tokens();
        let viewport = 400.0;
        // Content only 1px more than viewport — thumb should nearly fill track
        let geom = compute_scroll_indicator(viewport, viewport + 1.0, 0.0, &tokens).unwrap();
        assert!(
            geom.thumb_height_px <= geom.track_height_px + f32::EPSILON,
            "thumb height must not exceed track height"
        );
    }

    // ─── Scroll offset clamping ────────────────────────────────────────────

    #[test]
    fn negative_scroll_offset_clamped_to_zero() {
        let tokens = default_tokens();
        let geom = compute_scroll_indicator(400.0, 1200.0, -100.0, &tokens).unwrap();
        assert!(
            geom.thumb_y_px.abs() < 1.0,
            "negative scroll offset must clamp to 0 (thumb at top)"
        );
    }

    #[test]
    fn overshoot_scroll_offset_clamped_to_max() {
        let tokens = default_tokens();
        let viewport = 400.0;
        let content = 1200.0;
        let over_scroll = content + 500.0; // way beyond max
        let geom = compute_scroll_indicator(viewport, content, over_scroll, &tokens).unwrap();
        let track_bottom = geom.track_height_px;
        let thumb_bottom = geom.thumb_y_px + geom.thumb_height_px;
        assert!(
            (thumb_bottom - track_bottom).abs() < 1.0,
            "overshoot offset must clamp to max scroll (thumb at bottom)"
        );
    }

    // ─── Width from tokens ─────────────────────────────────────────────────

    #[test]
    fn geometry_width_matches_token_width() {
        let mut tokens = default_tokens();
        tokens.width_px = 6.0; // custom width
        let geom = compute_scroll_indicator(400.0, 1200.0, 0.0, &tokens).unwrap();
        assert!(
            (geom.width_px - 6.0).abs() < f32::EPSILON,
            "geometry width must match token width"
        );
    }

    // ─── Thumb stays within track bounds ──────────────────────────────────

    #[test]
    fn thumb_never_exits_track_bounds() {
        // Property: for any combination of scroll offset in [0, overflow],
        // thumb_y >= 0 and thumb_y + thumb_height <= track_height.
        let tokens = default_tokens();
        let viewport = 400.0;
        let content = 1000.0;
        let overflow = content - viewport;

        for i in 0..=20 {
            let offset = overflow * (i as f32 / 20.0);
            let geom = compute_scroll_indicator(viewport, content, offset, &tokens).unwrap();
            assert!(
                geom.thumb_y_px >= 0.0,
                "thumb_y must be non-negative at offset {offset}"
            );
            assert!(
                geom.thumb_y_px + geom.thumb_height_px <= geom.track_height_px + f32::EPSILON,
                "thumb must not exit track bottom at offset {offset}"
            );
        }
    }

    // ─── Redaction safety ─────────────────────────────────────────────────

    /// Verify that `ScrollIndicatorGeometry` carries no content — only
    /// geometry values. This is a structural test: the struct fields must only
    /// contain numeric geometry, never strings or byte slices that could
    /// contain transcript content.
    ///
    /// The actual redaction guarantee is that callers never pass transcript
    /// text into this module; this test documents the contract structurally
    /// through JSON serialization verification.
    #[test]
    fn scroll_indicator_geometry_is_content_free() {
        let geom = ScrollIndicatorGeometry {
            thumb_y_px: 10.0,
            thumb_height_px: 50.0,
            track_height_px: 400.0,
            width_px: 4.0,
        };
        // Round-trip through serde_json to confirm the struct only
        // serialises geometry field names and numeric values.
        let json = serde_json::to_string(&geom).unwrap();
        // Must not contain any content-related keywords as field names or values.
        for word in ["transcript", "draft", "text", "content", "session"] {
            assert!(
                !json.contains(word),
                "geometry JSON must not contain content-related word '{word}'"
            );
        }
        let back: ScrollIndicatorGeometry = serde_json::from_str(&json).unwrap();
        assert_eq!(geom, back, "round-trip must preserve geometry");
    }
}
