//! # window
//!
//! Window mode abstraction.
//!
//! ## Two modes, one API
//!
//! - **Fullscreen**: compositor owns the entire display. Opaque background.
//!   All input captured. All platforms.
//! - **Overlay / HUD**: transparent, borderless, always-on-top window.
//!   Per-region input passthrough (pointer events outside active hit-regions
//!   pass through to the desktop). Platform-specific.
//!
//! Runtime mode switching is supported but disruptive — it requires surface
//! recreation.
//!

use std::fmt;

// ─── Window mode ─────────────────────────────────────────────────────────────

/// The configured window mode for the runtime.
///
/// Modes are set at startup. Switching at runtime is possible but requires
/// surface recreation (spec line 175).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowMode {
    /// Compositor owns the entire display. Opaque. All input captured.
    #[default]
    Fullscreen,
    /// Transparent borderless always-on-top window.
    /// Pointer events outside active hit-regions pass through to the desktop.
    Overlay,
}

impl fmt::Display for WindowMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WindowMode::Fullscreen => write!(f, "fullscreen"),
            WindowMode::Overlay => write!(f, "overlay"),
        }
    }
}

// ─── Input passthrough ────────────────────────────────────────────────────────

/// Represents a rectangular hit-region on screen for overlay input passthrough.
///
/// Pointer events within the union of all `HitRegion` bounds are captured by
/// the runtime. Events outside any hit-region are passed through to the desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct HitRegion {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl HitRegion {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Returns `true` if the point (px, py) is inside this region.
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
}

// ─── WindowConfig ─────────────────────────────────────────────────────────────

/// Complete window configuration for the runtime.
#[derive(Debug, Clone)]
pub struct WindowConfig {
    pub mode: WindowMode,
    pub width: u32,
    pub height: u32,
    /// Title used in non-fullscreen modes (for debugging, alt-tab, etc.).
    pub title: String,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            mode: WindowMode::Fullscreen,
            width: 1920,
            height: 1080,
            title: "tze_hud".to_string(),
        }
    }
}

// ─── Input passthrough logic ──────────────────────────────────────────────────

/// Decide whether a pointer event at (px, py) should be captured by the
/// runtime or passed through to the desktop.
///
/// Spec line 182: "WHEN runtime is in overlay mode and pointer event lands
/// outside any active hit-region THEN event MUST pass through to underlying
/// desktop."
///
/// In fullscreen mode, all events are always captured.
pub fn should_capture_pointer_event(
    mode: WindowMode,
    px: f32,
    py: f32,
    hit_regions: &[HitRegion],
) -> bool {
    match mode {
        WindowMode::Fullscreen => true,
        WindowMode::Overlay => hit_regions.iter().any(|r| r.contains(px, py)),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── WindowMode ──────────────────────────────────────────────────────────

    #[test]
    fn window_mode_default_is_fullscreen() {
        assert_eq!(WindowMode::default(), WindowMode::Fullscreen);
    }

    #[test]
    fn window_mode_display() {
        assert_eq!(format!("{}", WindowMode::Fullscreen), "fullscreen");
        assert_eq!(format!("{}", WindowMode::Overlay), "overlay");
    }

    // ── HitRegion ────────────────────────────────────────────────────────────

    #[test]
    fn hit_region_contains_interior_point() {
        let r = HitRegion::new(10.0, 20.0, 100.0, 50.0);
        assert!(r.contains(60.0, 40.0));
    }

    #[test]
    fn hit_region_does_not_contain_exterior_point() {
        let r = HitRegion::new(10.0, 20.0, 100.0, 50.0);
        assert!(!r.contains(5.0, 40.0)); // left of region
        assert!(!r.contains(115.0, 40.0)); // right of region
        assert!(!r.contains(60.0, 15.0)); // above region
        assert!(!r.contains(60.0, 75.0)); // below region
    }

    #[test]
    fn hit_region_top_left_inclusive() {
        let r = HitRegion::new(10.0, 20.0, 100.0, 50.0);
        assert!(r.contains(10.0, 20.0), "top-left corner is inclusive");
    }

    #[test]
    fn hit_region_bottom_right_exclusive() {
        let r = HitRegion::new(10.0, 20.0, 100.0, 50.0);
        // x=110, y=70 are exactly at width/height boundary (exclusive).
        assert!(!r.contains(110.0, 70.0), "boundary is exclusive");
    }

    // ── should_capture_pointer_event ─────────────────────────────────────────

    #[test]
    fn fullscreen_mode_captures_all_events() {
        let regions = vec![HitRegion::new(0.0, 0.0, 10.0, 10.0)];
        // Even outside any hit-region, fullscreen captures everything.
        assert!(should_capture_pointer_event(
            WindowMode::Fullscreen,
            9999.0,
            9999.0,
            &regions
        ));
    }

    #[test]
    fn overlay_mode_captures_event_inside_hit_region() {
        let regions = vec![HitRegion::new(50.0, 50.0, 200.0, 100.0)];
        assert!(should_capture_pointer_event(
            WindowMode::Overlay,
            100.0,
            80.0,
            &regions
        ));
    }

    #[test]
    fn overlay_mode_passes_through_event_outside_hit_region() {
        let regions = vec![HitRegion::new(50.0, 50.0, 200.0, 100.0)];
        // Point at (10, 10) is outside the hit-region — must pass through.
        assert!(!should_capture_pointer_event(
            WindowMode::Overlay,
            10.0,
            10.0,
            &regions
        ));
    }

    #[test]
    fn overlay_mode_no_hit_regions_passes_through_all_events() {
        let regions: Vec<HitRegion> = vec![];
        assert!(!should_capture_pointer_event(
            WindowMode::Overlay,
            100.0,
            100.0,
            &regions
        ));
    }

    #[test]
    fn overlay_mode_multiple_hit_regions_union() {
        let regions = vec![
            HitRegion::new(0.0, 0.0, 100.0, 100.0),
            HitRegion::new(200.0, 200.0, 100.0, 100.0),
        ];
        assert!(should_capture_pointer_event(
            WindowMode::Overlay,
            50.0,
            50.0,
            &regions
        ));
        assert!(should_capture_pointer_event(
            WindowMode::Overlay,
            250.0,
            250.0,
            &regions
        ));
        assert!(!should_capture_pointer_event(
            WindowMode::Overlay,
            150.0,
            150.0,
            &regions
        ));
    }

    // ── WindowConfig ─────────────────────────────────────────────────────────

    #[test]
    fn window_config_default() {
        let cfg = WindowConfig::default();
        assert_eq!(cfg.mode, WindowMode::Fullscreen);
        assert_eq!(cfg.width, 1920);
        assert_eq!(cfg.height, 1080);
    }
}
