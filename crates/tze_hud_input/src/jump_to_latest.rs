//! Token-styled "jump to latest" affordance for scrolled-back portal tiles.
//!
//! Auto follow-tail already exists (`FollowTailAnchor` / `ScrollState`) and
//! `ScrollState::reset_to_tail` can snap a tile's viewport back to the tail,
//! but there was no user-facing control to trigger it — a viewer scrolled up
//! into transcript history had no way back to the live edge short of manually
//! scrolling all the way down (hud-9ci61).
//!
//! This module computes the geometry of a small clickable pill that:
//! - the compositor renders ONLY when a tile is scrolled away from the tail
//!   (`renderer/frame.rs`, alongside the existing scroll-position indicator),
//! - the compositor also hit-tests each frame
//!   (`renderer/hit_regions.rs::populate_zone_hit_regions`), and
//! - the runtime wires to `InputProcessor::reset_tile_scroll_to_tail` on
//!   click (`windowed/lifecycle.rs`, `headless.rs`).
//!
//! ## Design
//!
//! Token-styled: every visual property comes from [`JumpToLatestTokens`],
//! resolved by the compositor from the portal token map. No literal visual
//! values are permitted in the geometry computation itself.
//!
//! Ambient/subtle: this is a presence engine, not a chat app — the pill is a
//! small fixed-size affordance anchored to the tile, not a loud banner.
//! Placement (bottom-center) and appearance (a plain filled rect, no icon)
//! are intentionally minimal for this first pass; the owner may refine
//! sizing, iconography, or position in a follow-up.
//!
//! ## Ambient unread count (hud-g1ena.3)
//!
//! Per portal-chat-grade-affordances §Jump-to-Latest Affordance, the pill MAY
//! carry the ambient unread count. When the tile has a nonzero, non-redacted
//! unread-output count (plumbed onto the tile by the portal projection driver
//! from `PortalView::unread_output_count`), the compositor renders
//! the [`jump_to_latest_badge_label`] centered in the pill. The count is a
//! quiet, token-styled label — never a notification; it updates in place as the
//! backlog grows and clears with the pill itself the instant the viewer returns
//! to the tail (the pill is gated on `scrolled_back`). The count is presentation
//! of runtime-owned state; no adapter cooperation is involved in showing it, and
//! (as with the whole affordance) an adapter can never trigger the jump.
//!
//! ## Local feedback first
//!
//! The click handler calls `ScrollState::reset_to_tail` (via
//! `InputProcessor::reset_tile_scroll_to_tail`) synchronously in the same
//! pointer-up dispatch that produced the hit — no roundtrip to an adapter.

use serde::{Deserialize, Serialize};

/// sRGB transfer: straight-sRGB component → linear, for one channel in [0.0, 1.0].
///
/// Kept byte-for-byte identical to `tze_hud_compositor`'s `srgb_to_linear` so the
/// `color_*` DEFAULT below lands in the same LINEAR space the token-set path
/// produces (that path parses `#RRGGBB` via the compositor's `parse_hex_color`,
/// which applies this same transfer). No compile-time link between the two copies;
/// update both if the transfer function ever changes.
#[inline]
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Token-resolved visual properties for the "jump to latest" pill.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JumpToLatestTokens {
    /// Pill fill color. LINEAR RGB components in [0.0, 1.0] (unlike `text_*`,
    /// which are STRAIGHT sRGB). The compositor feeds these straight into
    /// `gpu_color_raw`, which assumes linear input; the token-set path resolves
    /// `portal.jump_to_latest.color` through `parse_hex_color` (sRGB→linear), so
    /// the DEFAULT below must also be linear for default and token-set to render
    /// the same intended color (hud-25pee). Alpha carries no color-space transform.
    pub color_r: f32,
    pub color_g: f32,
    pub color_b: f32,
    pub color_a: f32,
    /// Ambient unread-count badge text color. STRAIGHT (non-linear) sRGB
    /// components in [0.0, 1.0], matching the `color_*` default convention above;
    /// the compositor encodes these to sRGB u8 by a plain scale. Used only when
    /// the pill carries an unread count (hud-g1ena.3); the plain pill (no unread)
    /// draws no text.
    pub text_r: f32,
    pub text_g: f32,
    pub text_b: f32,
    pub text_a: f32,
    /// Font size (px) for the unread-count badge text.
    pub text_size_px: f32,
    /// Pill width in pixels (clamped to the tile width at render time).
    pub width_px: f32,
    /// Pill height in pixels (clamped to the tile height at render time).
    pub height_px: f32,
    /// Margin (px) between the pill's bottom edge and the tile's bottom edge.
    pub margin_px: f32,
}

impl Default for JumpToLatestTokens {
    fn default() -> Self {
        // Must match `token_colors::resolve_jump_to_latest_tokens` fallback
        // defaults in tze_hud_compositor. No compile-time link; update both
        // sides when changing defaults.
        Self {
            // #4A5568 — the same neutral chrome tone as the scroll indicator
            // thumb, ambient rather than alarming. Stored LINEAR (via
            // srgb_to_linear) to match the token-set path, which parses the same
            // hex through the compositor's parse_hex_color (sRGB→linear); the
            // compositor's gpu_color_raw consumes color_* as linear (hud-25pee).
            color_r: srgb_to_linear(0x4A as f32 / 255.0),
            color_g: srgb_to_linear(0x55 as f32 / 255.0),
            color_b: srgb_to_linear(0x68 as f32 / 255.0),
            color_a: 0.9,
            // #CBD5E0 — a light neutral that reads on the #4A5568 pill without
            // the loudness of pure white; ambient, subordinate to content.
            text_r: 0xCB as f32 / 255.0,
            text_g: 0xD5 as f32 / 255.0,
            text_b: 0xE0 as f32 / 255.0,
            text_a: 0.95,
            text_size_px: 13.0,
            width_px: 96.0,
            height_px: 24.0,
            margin_px: 8.0,
        }
    }
}

/// Format the ambient unread-count badge the jump-to-latest pill MAY carry
/// (portal-chat-grade-affordances §Jump-to-Latest Affordance: "The affordance
/// MAY carry the ambient unread count").
///
/// The input is the runtime-owned aggregate unread-output count already tracked
/// end-to-end (`PortalView::unread_output_count`, plumbed onto the
/// tile by the portal projection driver). It is `None` when the count is
/// redacted by the authority's `reveal_unread` policy.
///
/// Returns `None` — render no badge, leaving the plain pill — when:
/// - the count is redacted (`None`), or
/// - there is nothing unread (`Some(0)`) — a presence engine renders nothing
///   rather than a "0 unread" marker.
///
/// This matches the gating of the in-transcript ambient unread indicator
/// (`tze_hud_projection::resident_grpc::unread_indicator_line`) so the pill
/// badge and the ambient count agree. Large counts clamp to `"999+ unread"` so
/// the label never overflows the compact pill; the compositor additionally
/// clips to the pill interior.
pub fn jump_to_latest_badge_label(unread_count: Option<usize>) -> Option<String> {
    match unread_count {
        Some(count) if count > 999 => Some("999+ unread".to_string()),
        Some(count) if count > 0 => Some(format!("{count} unread")),
        _ => None,
    }
}

/// Computed geometry for the "jump to latest" pill, in tile-local pixels
/// (origin = tile top-left, matching [`crate::ScrollIndicatorGeometry`]'s
/// convention).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct JumpToLatestGeometry {
    /// X offset of the pill's left edge within the tile.
    pub x_px: f32,
    /// Y offset of the pill's top edge within the tile.
    pub y_px: f32,
    /// Pill width.
    pub width_px: f32,
    /// Pill height.
    pub height_px: f32,
}

/// Compute "jump to latest" pill geometry for a single tile.
///
/// Returns `None` whenever the pill must not be shown:
/// - `scrolled_back == false` (the tile is already at the tail), or
/// - the viewport dimensions are non-positive or non-finite.
///
/// Callers MUST additionally gate on content overflow — e.g. only call this
/// when [`crate::compute_scroll_indicator`] also returns `Some` for the same
/// tile — there is no point offering to "jump to latest" content that
/// already fits entirely in the viewport (scroll state can never report
/// `ScrolledBack` for such a tile in practice, but callers should not rely on
/// that as the sole guard).
///
/// # Arguments
///
/// * `viewport_w_px` / `viewport_h_px` — visible size of the tile.
/// * `scrolled_back` — `true` when the tile's `FollowTailAnchor` is
///   `ScrolledBack` (the viewer has scrolled away from the tail).
/// * `tokens` — resolved pill styling from the portal token set.
pub fn compute_jump_to_latest_pill(
    viewport_w_px: f32,
    viewport_h_px: f32,
    scrolled_back: bool,
    tokens: &JumpToLatestTokens,
) -> Option<JumpToLatestGeometry> {
    if !scrolled_back {
        return None;
    }
    // Reject non-finite inputs before any comparison (NaN comparisons are
    // always false, so a `<= 0.0` check alone would silently pass for NaN).
    if !viewport_w_px.is_finite() || !viewport_h_px.is_finite() {
        return None;
    }
    if viewport_w_px <= 0.0 || viewport_h_px <= 0.0 {
        return None;
    }

    let width = tokens.width_px.min(viewport_w_px).max(0.0);
    let height = tokens.height_px.min(viewport_h_px).max(0.0);
    // Bottom-center: ambient, and out of the way of the scroll indicator
    // thumb (which tracks the tile's right edge).
    let x = ((viewport_w_px - width) / 2.0).max(0.0);
    let y = (viewport_h_px - height - tokens.margin_px).max(0.0);

    Some(JumpToLatestGeometry {
        x_px: x,
        y_px: y,
        width_px: width,
        height_px: height,
    })
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_tokens() -> JumpToLatestTokens {
        JumpToLatestTokens::default()
    }

    // ─── Color-space convention (hud-25pee) ───────────────────────────────

    /// The pill FILL default is stored LINEAR — the space `gpu_color_raw`
    /// consumes and the space the token-set path (`parse_hex_color`) produces —
    /// so default and token-set render the same intended color. It must equal
    /// the straight-sRGB #4A5568 channels passed through `srgb_to_linear`, and be
    /// strictly below those straight values (a plain literal would be the bug).
    #[test]
    fn pill_fill_default_is_linear() {
        let d = default_tokens();
        let (sr, sg, sb) = (
            0x4A as f32 / 255.0,
            0x55 as f32 / 255.0,
            0x68 as f32 / 255.0,
        );
        let eps = 1e-6;
        assert!((d.color_r - srgb_to_linear(sr)).abs() < eps);
        assert!((d.color_g - srgb_to_linear(sg)).abs() < eps);
        assert!((d.color_b - srgb_to_linear(sb)).abs() < eps);
        // Linear is strictly darker than the straight-sRGB literal for these mid
        // tones — the regression lock against reintroducing the raw literal.
        assert!(d.color_r < sr - eps && d.color_g < sg - eps && d.color_b < sb - eps);
        // `text_*`, by contrast, remain STRAIGHT sRGB (different consumer path).
        assert_eq!(d.text_r, 0xCB as f32 / 255.0);
    }

    // ─── Visibility gates on scrolled-back state ──────────────────────────

    #[test]
    fn hidden_when_at_tail() {
        let tokens = default_tokens();
        let result = compute_jump_to_latest_pill(400.0, 300.0, false, &tokens);
        assert!(
            result.is_none(),
            "pill must be hidden when the tile is at the tail (not scrolled back)"
        );
    }

    #[test]
    fn shown_when_scrolled_back() {
        let tokens = default_tokens();
        let result = compute_jump_to_latest_pill(400.0, 300.0, true, &tokens);
        assert!(
            result.is_some(),
            "pill must appear when the tile is scrolled back"
        );
    }

    // ─── Geometry ──────────────────────────────────────────────────────────

    #[test]
    fn pill_anchored_bottom_center() {
        let tokens = default_tokens();
        let geom = compute_jump_to_latest_pill(400.0, 300.0, true, &tokens).unwrap();
        let expected_x = (400.0 - geom.width_px) / 2.0;
        let expected_y = 300.0 - geom.height_px - tokens.margin_px;
        assert!(
            (geom.x_px - expected_x).abs() < f32::EPSILON,
            "pill must be horizontally centered"
        );
        assert!(
            (geom.y_px - expected_y).abs() < f32::EPSILON,
            "pill must sit `margin_px` above the tile's bottom edge"
        );
    }

    #[test]
    fn pill_size_matches_tokens() {
        let mut tokens = default_tokens();
        tokens.width_px = 50.0;
        tokens.height_px = 20.0;
        let geom = compute_jump_to_latest_pill(400.0, 300.0, true, &tokens).unwrap();
        assert!((geom.width_px - 50.0).abs() < f32::EPSILON);
        assert!((geom.height_px - 20.0).abs() < f32::EPSILON);
    }

    #[test]
    fn pill_clamped_to_tiny_viewport() {
        let tokens = default_tokens(); // width=96, height=24
        let geom = compute_jump_to_latest_pill(40.0, 10.0, true, &tokens).unwrap();
        assert!(geom.width_px <= 40.0, "pill width must not exceed viewport");
        assert!(
            geom.height_px <= 10.0,
            "pill height must not exceed viewport"
        );
        assert!(geom.x_px >= 0.0);
        assert!(geom.y_px >= 0.0);
    }

    #[test]
    fn non_finite_viewport_rejected() {
        let tokens = default_tokens();
        assert!(compute_jump_to_latest_pill(f32::NAN, 300.0, true, &tokens).is_none());
        assert!(compute_jump_to_latest_pill(400.0, f32::INFINITY, true, &tokens).is_none());
    }

    #[test]
    fn non_positive_viewport_rejected() {
        let tokens = default_tokens();
        assert!(compute_jump_to_latest_pill(0.0, 300.0, true, &tokens).is_none());
        assert!(compute_jump_to_latest_pill(400.0, -1.0, true, &tokens).is_none());
    }

    // ─── Ambient unread-count badge (hud-g1ena.3) ─────────────────────────

    #[test]
    fn badge_hidden_when_redacted() {
        // `None` = the authority's reveal_unread policy withheld the count.
        assert_eq!(jump_to_latest_badge_label(None), None);
    }

    #[test]
    fn badge_hidden_when_zero() {
        // A presence engine renders nothing rather than a "0 unread" marker,
        // matching the ambient in-transcript indicator's gating.
        assert_eq!(jump_to_latest_badge_label(Some(0)), None);
    }

    #[test]
    fn badge_shows_count_when_unread() {
        assert_eq!(
            jump_to_latest_badge_label(Some(1)).as_deref(),
            Some("1 unread")
        );
        assert_eq!(
            jump_to_latest_badge_label(Some(42)).as_deref(),
            Some("42 unread")
        );
        assert_eq!(
            jump_to_latest_badge_label(Some(999)).as_deref(),
            Some("999 unread")
        );
    }

    #[test]
    fn badge_clamps_large_counts() {
        // Beyond 999 the label caps so it never overflows the compact pill.
        assert_eq!(
            jump_to_latest_badge_label(Some(1000)).as_deref(),
            Some("999+ unread")
        );
        assert_eq!(
            jump_to_latest_badge_label(Some(1_000_000)).as_deref(),
            Some("999+ unread")
        );
    }

    #[test]
    fn tokens_carry_badge_text_style() {
        // The badge text color/size travel on the same token struct as the pill
        // fill so the compositor resolves both from `portal.jump_to_latest.*`.
        let tokens = default_tokens();
        assert!(tokens.text_a > 0.0, "badge text must be visible by default");
        assert!(
            tokens.text_size_px > 0.0,
            "badge font size must be positive"
        );
    }
}
