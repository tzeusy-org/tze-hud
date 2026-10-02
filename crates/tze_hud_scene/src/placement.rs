//! Tile placement: resolve an agent's anchor + size-class hint to a rect.
//!
//! Agents never send geometry (`docs/api.md` decision 2). `ClaimTile` names
//! an anchor and a size class; the runtime turns them into bounds using
//! [`TilePlacementTokens`] (built from `[design_tokens]` `tile.*`), steps
//! past tiles already at that spot so claims stack deterministically, and
//! clamps the result to the display.

use crate::types::Rect;

/// Where on the display a claimed tile sits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TileAnchor {
    TopLeft,
    Top,
    #[default]
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// A named tile size; the pixel sizes come from design tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TileSize {
    Small,
    #[default]
    Medium,
    Large,
    Wide,
    Tall,
}

/// Pixel sizes for each size class, the display-edge margin, and the gap
/// between stacked tiles. Defaults match the canonical `tile.*` tokens.
#[derive(Clone, Debug, PartialEq)]
pub struct TilePlacementTokens {
    pub small: (f32, f32),
    pub medium: (f32, f32),
    pub large: (f32, f32),
    pub wide: (f32, f32),
    pub tall: (f32, f32),
    pub margin: f32,
    pub gap: f32,
}

impl Default for TilePlacementTokens {
    fn default() -> Self {
        Self {
            small: (240.0, 120.0),
            medium: (360.0, 220.0),
            large: (560.0, 360.0),
            wide: (720.0, 120.0),
            tall: (300.0, 520.0),
            margin: 24.0,
            gap: 12.0,
        }
    }
}

impl TilePlacementTokens {
    pub fn size(&self, size: TileSize) -> (f32, f32) {
        match size {
            TileSize::Small => self.small,
            TileSize::Medium => self.medium,
            TileSize::Large => self.large,
            TileSize::Wide => self.wide,
            TileSize::Tall => self.tall,
        }
    }
}

/// Resolve a placement to bounds inside `display`.
///
/// The tile starts at the anchor's spot (inset by `margin`). While it
/// overlaps one of `occupied`, it moves past that tile plus `gap`: down for
/// top and middle-row anchors, up for bottom-row anchors. The size is clamped
/// to the display minus margins, and the final rect to the display.
pub fn resolve_tile_placement(
    tokens: &TilePlacementTokens,
    anchor: TileAnchor,
    size: TileSize,
    display: Rect,
    occupied: &[Rect],
) -> Rect {
    use TileAnchor as A;
    let m = tokens.margin;
    let (w, h) = tokens.size(size);
    let w = w.min((display.width - 2.0 * m).max(1.0));
    let h = h.min((display.height - 2.0 * m).max(1.0));
    let x = match anchor {
        A::TopLeft | A::Left | A::BottomLeft => display.x + m,
        A::Top | A::Center | A::Bottom => display.x + (display.width - w) / 2.0,
        A::TopRight | A::Right | A::BottomRight => display.x + display.width - m - w,
    };
    let mut y = match anchor {
        A::TopLeft | A::Top | A::TopRight => display.y + m,
        A::Left | A::Center | A::Right => display.y + (display.height - h) / 2.0,
        A::BottomLeft | A::Bottom | A::BottomRight => display.y + display.height - m - h,
    };
    let upward = matches!(anchor, A::BottomLeft | A::Bottom | A::BottomRight);
    // Each step clears at least one occupied rect, so this terminates.
    for _ in 0..=occupied.len() {
        let candidate = Rect::new(x, y, w, h);
        let Some(hit) = occupied.iter().find(|r| r.intersects(&candidate)) else {
            break;
        };
        y = if upward {
            hit.y - tokens.gap - h
        } else {
            hit.y + hit.height + tokens.gap
        };
    }
    let max_y = display.y + display.height - h;
    Rect::new(x, y.clamp(display.y, max_y.max(display.y)), w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display() -> Rect {
        Rect::new(0.0, 0.0, 1920.0, 1080.0)
    }

    #[test]
    fn anchors_place_inside_margins() {
        let t = TilePlacementTokens::default();
        let r = resolve_tile_placement(&t, TileAnchor::TopRight, TileSize::Medium, display(), &[]);
        assert_eq!(r, Rect::new(1920.0 - 24.0 - 360.0, 24.0, 360.0, 220.0));
        let r = resolve_tile_placement(&t, TileAnchor::Center, TileSize::Small, display(), &[]);
        assert_eq!(r, Rect::new(840.0, 480.0, 240.0, 120.0));
        let r = resolve_tile_placement(&t, TileAnchor::BottomLeft, TileSize::Small, display(), &[]);
        assert_eq!(r, Rect::new(24.0, 1080.0 - 24.0 - 120.0, 240.0, 120.0));
    }

    #[test]
    fn same_anchor_claims_stack_deterministically() {
        let t = TilePlacementTokens::default();
        let first =
            resolve_tile_placement(&t, TileAnchor::TopRight, TileSize::Small, display(), &[]);
        let second = resolve_tile_placement(
            &t,
            TileAnchor::TopRight,
            TileSize::Small,
            display(),
            &[first],
        );
        assert_eq!(second.y, first.y + first.height + t.gap);
        let third = resolve_tile_placement(
            &t,
            TileAnchor::TopRight,
            TileSize::Small,
            display(),
            &[first, second],
        );
        assert_eq!(third.y, second.y + second.height + t.gap);

        let low = resolve_tile_placement(&t, TileAnchor::Bottom, TileSize::Small, display(), &[]);
        let above =
            resolve_tile_placement(&t, TileAnchor::Bottom, TileSize::Small, display(), &[low]);
        assert_eq!(above.y, low.y - t.gap - above.height);
    }

    #[test]
    fn oversized_and_overflowing_tiles_are_clamped() {
        let t = TilePlacementTokens::default();
        let small = Rect::new(0.0, 0.0, 400.0, 300.0);
        let r = resolve_tile_placement(&t, TileAnchor::TopLeft, TileSize::Large, small, &[]);
        assert!(r.width <= 400.0 - 48.0 && r.height <= 300.0 - 48.0);
        let full: Vec<Rect> = (0..20)
            .map(|i| Rect::new(0.0, i as f32 * 60.0, 1920.0, 60.0))
            .collect();
        let r = resolve_tile_placement(&t, TileAnchor::Top, TileSize::Small, display(), &full);
        assert!(r.y >= 0.0 && r.y + r.height <= 1080.0);
    }
}
