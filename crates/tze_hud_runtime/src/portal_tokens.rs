//! Production bridge: `PortalPartTokens` → `PortalVisualTokens`.
//!
//! The canonical conversion function lives in `tze_hud_projection::resident_grpc`
//! so that it can be used directly from the projection authority binary without
//! pulling in the full runtime crate (which would create a circular dependency:
//! `tze_hud_runtime` already depends on `tze_hud_projection`).
//!
//! This module re-exports `portal_visual_tokens_from_part_tokens` for consumers
//! that import from `tze_hud_runtime`.
//!
//! ## Production wiring contract
//!
//! Any code that constructs a `ResidentGrpcPortalAdapter` MUST call
//! `portal_visual_tokens_from_part_tokens` instead of hand-constructing
//! `PortalVisualTokens`. When a token-map swap occurs (e.g. profile hot-reload
//! via `compositor.set_token_map`), call `resolve_portal_tokens` on the new
//! `DesignTokenMap` to get `PortalPartTokens`, then pass the result to this
//! function, and forward the resulting `PortalVisualTokens` to
//! `adapter.set_visual_tokens(...)`.
//!
//! ```rust,ignore
//! use tze_hud_config::{resolve_portal_tokens, tokens::DesignTokenMap};
//! use tze_hud_projection::resident_grpc::{ResidentGrpcPortalAdapter, ResidentGrpcPortalConfig};
//! use tze_hud_runtime::portal_tokens::portal_visual_tokens_from_part_tokens;
//!
//! // At adapter construction:
//! let part_tokens = resolve_portal_tokens(&resolved_token_map);
//! let visual_tokens = portal_visual_tokens_from_part_tokens(&part_tokens);
//! let adapter = ResidentGrpcPortalAdapter::with_tokens(config, visual_tokens);
//!
//! // On profile hot-reload:
//! let new_part_tokens = resolve_portal_tokens(&new_token_map);
//! adapter.set_visual_tokens(portal_visual_tokens_from_part_tokens(&new_part_tokens));
//! ```

pub use tze_hud_projection::resident_grpc::portal_visual_tokens_from_part_tokens;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_config::resolve_portal_tokens;
    use tze_hud_projection::resident_grpc::PortalVisualTokens;

    /// Single-source-of-truth assertion (hud-dcynv): `PortalVisualTokens::default()` now
    /// derives from `tze_hud_config::PortalPartTokens::default()` via
    /// `portal_visual_tokens_from_part_tokens`, so both sides MUST agree exactly.
    ///
    /// Previously `PortalVisualTokens::default()` contained hand-coded floats that
    /// diverged from the config defaults by up to 3 ULPs of rounding. This test
    /// enforces the post-consolidation invariant: there is one canonical palette,
    /// and any future change to `tze_hud_config::portal_tokens::defaults` propagates
    /// here automatically — a change to one side that diverges from the other will
    /// fail this test.
    #[test]
    fn portal_visual_defaults_are_single_source_of_truth() {
        let empty: tze_hud_config::tokens::DesignTokenMap = std::collections::HashMap::new();
        let part_tokens = resolve_portal_tokens(&empty);
        // This is the "config path": config defaults → conversion function
        let from_config = portal_visual_tokens_from_part_tokens(&part_tokens);
        // This is the "projection default path": PortalVisualTokens::default()
        // which now delegates to portal_visual_tokens_from_part_tokens(&PortalPartTokens::default())
        let default_direct = PortalVisualTokens::default();

        // They must be exactly equal — same code path, same inputs.
        assert_eq!(
            from_config, default_direct,
            "PortalVisualTokens::default() must be exactly equal to \
             portal_visual_tokens_from_part_tokens(PortalPartTokens::default()); \
             divergence means the single-source-of-truth invariant was broken"
        );
    }

    /// Round-trip: `resolve_portal_tokens` → `portal_visual_tokens_from_part_tokens`
    /// must yield the same transcript/collapsed values as `PortalVisualTokens::default`.
    ///
    /// This verifies that the default-palette constants in both crates match the
    /// resolved defaults coming through `tze_hud_config::resolve_portal_tokens`.
    #[test]
    fn default_part_tokens_round_trip_matches_visual_defaults() {
        let empty: tze_hud_config::tokens::DesignTokenMap = std::collections::HashMap::new();
        let part_tokens = resolve_portal_tokens(&empty);
        let visual = portal_visual_tokens_from_part_tokens(&part_tokens);
        let default = PortalVisualTokens::default();

        // Transcript background (all four channels, including alpha)
        let eps = 1e-2_f32;
        assert!(
            (visual.transcript_background.r - default.transcript_background.r).abs() < eps,
            "transcript_background.r mismatch: got {}, expected {}",
            visual.transcript_background.r,
            default.transcript_background.r
        );
        assert!(
            (visual.transcript_background.g - default.transcript_background.g).abs() < eps,
            "transcript_background.g mismatch"
        );
        assert!(
            (visual.transcript_background.b - default.transcript_background.b).abs() < eps,
            "transcript_background.b mismatch"
        );
        assert!(
            (visual.transcript_background.a - default.transcript_background.a).abs() < eps,
            "transcript_background.a mismatch: got {}, expected {}",
            visual.transcript_background.a,
            default.transcript_background.a
        );

        // Transcript text color
        assert!(
            (visual.transcript_text_color.r - default.transcript_text_color.r).abs() < eps,
            "transcript_text_color.r mismatch: got {}, expected {}",
            visual.transcript_text_color.r,
            default.transcript_text_color.r
        );

        // Collapsed background (all four channels, including alpha)
        assert!(
            (visual.collapsed_background.r - default.collapsed_background.r).abs() < eps,
            "collapsed_background.r mismatch: got {}, expected {}",
            visual.collapsed_background.r,
            default.collapsed_background.r
        );
        assert!(
            (visual.collapsed_background.a - default.collapsed_background.a).abs() < eps,
            "collapsed_background.a mismatch: got {}, expected {}",
            visual.collapsed_background.a,
            default.collapsed_background.a
        );

        // Font sizes
        assert!(
            (visual.transcript_font_size_px - default.transcript_font_size_px).abs() < eps,
            "transcript_font_size_px mismatch"
        );
        assert!(
            (visual.collapsed_font_size_px - default.collapsed_font_size_px).abs() < eps,
            "collapsed_font_size_px mismatch"
        );

        // Composer fields
        assert!(
            (visual.composer_font_size_px - default.composer_font_size_px).abs() < eps,
            "composer_font_size_px mismatch"
        );
        // NOTE: the composer at-capacity color is no longer mirrored on
        // PortalVisualTokens -- it is resolved + applied compositor-side
        // (`portal.composer.at_capacity_color` -> `composer_draft_base_color`),
        // and its non-zero-alpha default is asserted in tze_hud_config's
        // portal_tokens tests (hud-9gyao).
    }

    /// Profile override propagates end-to-end through the canonical conversion chain.
    #[test]
    fn profile_override_propagates_through_conversion() {
        use tze_hud_config::tokens::resolve_tokens;
        use tze_hud_config::{
            PORTAL_TOKEN_COLLAPSED_FONT_SIZE, PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR,
        };

        let empty: tze_hud_config::tokens::DesignTokenMap = std::collections::HashMap::new();

        // Baseline
        let baseline_part = resolve_portal_tokens(&empty);
        let baseline = portal_visual_tokens_from_part_tokens(&baseline_part);

        // Override transcript text color and collapsed font size
        let mut overrides = tze_hud_config::tokens::DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR.to_string(),
            "#FF0000".to_string(), // red sentinel
        );
        overrides.insert(
            PORTAL_TOKEN_COLLAPSED_FONT_SIZE.to_string(),
            "20".to_string(),
        );
        let resolved = resolve_tokens(&empty, &overrides);
        let override_part = resolve_portal_tokens(&resolved);
        let overridden = portal_visual_tokens_from_part_tokens(&override_part);

        // Transcript text color must change to red
        assert!(
            (overridden.transcript_text_color.r - 1.0).abs() < 1e-3,
            "overridden transcript text color must have r=1.0 (red)"
        );
        assert!(
            overridden.transcript_text_color.g.abs() < 1e-3,
            "overridden transcript text color must have g=0.0"
        );
        assert!(
            overridden.transcript_text_color.b.abs() < 1e-3,
            "overridden transcript text color must have b=0.0"
        );

        // Collapsed font size must change
        assert!(
            (overridden.collapsed_font_size_px - 20.0).abs() < 1e-4,
            "overridden collapsed font size must be 20px"
        );

        // Baseline transcript text color must differ
        assert_ne!(
            baseline.transcript_text_color.r, overridden.transcript_text_color.r,
            "baseline and overridden transcript text colors must differ"
        );
    }
}
