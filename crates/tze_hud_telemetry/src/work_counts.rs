//! Dev-mode work counters for one rendered frame.
//!
//! Idle costs nothing and work is proportional to change: these counters let a
//! test (or a developer) see how much a frame repainted. They are plain
//! observations, not a certification artifact.

use serde::{Deserialize, Serialize};

/// What one rendered frame repainted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkCounts {
    /// Tiles whose background and content were re-encoded.
    pub tiles_redrawn: u32,
    /// Pixels inside the repaint (scissor) region.
    pub pixels_damaged: u64,
    /// True when the whole surface was repainted rather than a scoped region.
    pub full_frame: bool,
}

impl WorkCounts {
    /// Counts for a full-surface repaint of `tiles` visible tiles.
    pub fn full_frame(tiles: u32, width: u32, height: u32) -> Self {
        Self {
            tiles_redrawn: tiles,
            pixels_damaged: u64::from(width) * u64::from(height),
            full_frame: true,
        }
    }

    /// Counts for a scoped repaint of `tiles` tiles over `pixels_damaged` pixels.
    pub fn scoped(tiles: u32, pixels_damaged: u64) -> Self {
        Self {
            tiles_redrawn: tiles,
            pixels_damaged,
            full_frame: false,
        }
    }
}
