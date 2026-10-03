//! Safe-mode chrome overlay for the windowed frame path (hud-jm8nq.10).
//!
//! While safe mode is active the runtime paints a full-surface dim plus a top
//! banner bar above everything else, so the human can see the HUD is paused.
//! Colors and sizes come from `safe_mode.*` design tokens; the fallbacks below
//! MUST stay in sync with `tze_hud_config`'s `CANONICAL_TOKENS` (the crates are
//! intentionally unlinked).

use std::collections::HashMap;

use super::token_colors::{parse_hex_color, resolve_token_color};
use super::*;

const DIM_DEFAULT_COLOR_HEX: &str = "#000000B3";
const BANNER_DEFAULT_COLOR_HEX: &str = "#FFB800";
const BANNER_DEFAULT_HEIGHT_PX: f32 = 8.0;

/// One overlay rectangle in surface pixels with its straight-alpha color.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct OverlayRect {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) w: f32,
    pub(super) h: f32,
    pub(super) color: [f32; 4],
}

/// Back-to-front rectangles of the safe-mode overlay for a `w` x `h` surface.
pub(super) fn safe_mode_overlay_rects(
    token_map: &HashMap<String, String>,
    w: f32,
    h: f32,
) -> Vec<OverlayRect> {
    let color = |key: &str, fallback: &str| {
        resolve_token_color(token_map, key)
            .or_else(|| parse_hex_color(fallback))
            .unwrap_or(Rgba::WHITE)
            .to_array()
    };
    let banner_h = token_map
        .get("safe_mode.banner.height_px")
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(BANNER_DEFAULT_HEIGHT_PX)
        .min(h);
    vec![
        OverlayRect {
            x: 0.0,
            y: 0.0,
            w,
            h,
            color: color("safe_mode.overlay.color", DIM_DEFAULT_COLOR_HEX),
        },
        OverlayRect {
            x: 0.0,
            y: 0.0,
            w,
            h: banner_h,
            color: color("safe_mode.banner.color", BANNER_DEFAULT_COLOR_HEX),
        },
    ]
}

impl Compositor {
    /// Mirror the runtime's safe-mode flag; returns true when it changed (the
    /// caller must then repaint once, since no scene change accompanies it).
    pub fn set_safe_mode_overlay(&mut self, active: bool) -> bool {
        std::mem::replace(&mut self.safe_mode_overlay, active) != active
    }

    /// Overlay quads for the chrome pass; empty unless safe mode is active.
    pub(super) fn safe_mode_overlay_vertices(&self, sw: f32, sh: f32) -> Vec<RectVertex> {
        if !self.safe_mode_overlay {
            return Vec::new();
        }
        safe_mode_overlay_rects(&self.token_map, sw, sh)
            .iter()
            .flat_map(|r| rect_vertices(r.x, r.y, r.w, r.h, sw, sh, self.gpu_color_raw(r.color)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The overlay is token-driven: defaults cover the surface, tokens override.
    #[test]
    fn windowed_frame_draws_chrome_overlay_in_safe_mode() {
        let rects = safe_mode_overlay_rects(&HashMap::new(), 1920.0, 1080.0);
        assert_eq!(rects.len(), 2);
        assert_eq!(
            (rects[0].w, rects[0].h),
            (1920.0, 1080.0),
            "dim covers the surface"
        );
        assert!(
            rects[0].color[3] > 0.5 && rects[0].color[3] < 1.0,
            "dim is translucent"
        );
        assert_eq!((rects[1].w, rects[1].h), (1920.0, BANNER_DEFAULT_HEIGHT_PX));

        let tokens = HashMap::from([
            ("safe_mode.overlay.color".to_string(), "#FF0000".to_string()),
            ("safe_mode.banner.height_px".to_string(), "20".to_string()),
        ]);
        let rects = safe_mode_overlay_rects(&tokens, 800.0, 600.0);
        assert_eq!(
            rects[0].color,
            [1.0, 0.0, 0.0, 1.0],
            "color token overrides default"
        );
        assert_eq!(rects[1].h, 20.0, "height token overrides default");
    }
}
