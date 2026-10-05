//! Multi-display layout.
//!
//! The windowed runtime opens one overlay window per monitor, all showing one
//! shared scene. Scene coordinates are physical desktop pixels relative to the
//! primary monitor's top-left corner, so the primary display is the rect
//! `(0, 0, w, h)` (the scene's `display_area`) and every other display sits at
//! its desktop offset — possibly negative.
//!
//! [`DisplayLayout`] answers which display a zone resolves its geometry against
//! (config assigns zones by display name; anything unassigned, or assigned to a
//! display that is not connected, stays on the primary). [`FrameTarget`] is one
//! window's view of the shared frame: the compositor builds a frame once in
//! primary-display ("canvas") pixels and maps it onto each target.

use std::collections::HashMap;

use tze_hud_scene::Rect;

/// Canonical form of a display name for matching config against the OS.
///
/// Windows reports names like `\\.\DISPLAY6`; config may say `DISPLAY6` or
/// `display6`. Strips the device-namespace prefix and uppercases.
pub fn normalize_display_name(name: &str) -> String {
    name.trim().trim_start_matches(r"\\.\").to_ascii_uppercase()
}

/// One connected display, in scene coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayRect {
    /// Normalized display name (see [`normalize_display_name`]).
    pub name: String,
    /// Display bounds in scene pixels (primary = `(0, 0, w, h)`).
    pub rect: Rect,
    pub primary: bool,
}

/// Connected displays plus the configured zone → display assignment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DisplayLayout {
    displays: Vec<DisplayRect>,
    /// Zone name → normalized display name.
    zone_displays: HashMap<String, String>,
}

impl DisplayLayout {
    pub fn new(displays: Vec<DisplayRect>, zone_displays: &HashMap<String, String>) -> Self {
        let displays = displays
            .into_iter()
            .map(|d| DisplayRect {
                name: normalize_display_name(&d.name),
                ..d
            })
            .collect();
        let zone_displays = zone_displays
            .iter()
            .map(|(zone, display)| (zone.clone(), normalize_display_name(display)))
            .collect();
        Self {
            displays,
            zone_displays,
        }
    }

    pub fn displays(&self) -> &[DisplayRect] {
        &self.displays
    }

    /// Bounds of the non-primary display `zone` is assigned to, when that
    /// display is connected. `None` means the zone resolves on the primary.
    pub fn zone_display_rect(&self, zone: &str) -> Option<Rect> {
        let wanted = self.zone_displays.get(zone)?;
        self.displays
            .iter()
            .find(|d| !d.primary && &d.name == wanted)
            .map(|d| d.rect)
    }
}

/// One window's view of the shared frame.
///
/// `x`/`y` are the window's origin in scene pixels; `width`/`height` its
/// physical size. The primary window is `(0, 0)` at canvas size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameTarget {
    pub x: f32,
    pub y: f32,
    pub width: u32,
    pub height: u32,
    /// Primary-display chrome (widgets, system card) draws only here.
    pub primary: bool,
}

impl FrameTarget {
    /// The primary display: the canvas itself.
    pub fn primary(width: u32, height: u32) -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width,
            height,
            primary: true,
        }
    }

    /// True when this target is exactly the canvas, so frame data needs no
    /// mapping.
    pub(crate) fn is_canvas(&self, canvas_w: u32, canvas_h: u32) -> bool {
        self.x == 0.0 && self.y == 0.0 && self.width == canvas_w && self.height == canvas_h
    }

    /// Map from canvas NDC (what `rect_vertices` produced at canvas size) to
    /// this target's NDC.
    pub(crate) fn ndc_affine(&self, canvas_w: u32, canvas_h: u32) -> NdcAffine {
        let (cw, ch) = (canvas_w.max(1) as f32, canvas_h.max(1) as f32);
        let (tw, th) = (self.width.max(1) as f32, self.height.max(1) as f32);
        let sx = cw / tw;
        let sy = ch / th;
        NdcAffine {
            sx,
            bx: sx - 2.0 * self.x / tw - 1.0,
            sy,
            by: 1.0 - sy + 2.0 * self.y / th,
        }
    }
}

/// Per-axis affine map between two NDC spaces.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NdcAffine {
    sx: f32,
    bx: f32,
    sy: f32,
    by: f32,
}

impl NdcAffine {
    pub(crate) fn apply(&self, [x, y]: [f32; 2]) -> [f32; 2] {
        [x * self.sx + self.bx, y * self.sy + self.by]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> DisplayLayout {
        DisplayLayout::new(
            vec![
                DisplayRect {
                    name: r"\\.\DISPLAY7".into(),
                    rect: Rect::new(0.0, 0.0, 3840.0, 2160.0),
                    primary: true,
                },
                DisplayRect {
                    name: r"\\.\DISPLAY6".into(),
                    rect: Rect::new(3857.0, -1079.0, 3840.0, 2160.0),
                    primary: false,
                },
            ],
            &HashMap::from([
                ("notification-area".to_string(), "display6".to_string()),
                ("subtitle".to_string(), "DISPLAY9".to_string()),
                ("status-bar".to_string(), "DISPLAY7".to_string()),
            ]),
        )
    }

    #[test]
    fn display_names_match_with_or_without_device_prefix() {
        assert_eq!(normalize_display_name(r"\\.\DISPLAY6"), "DISPLAY6");
        assert_eq!(normalize_display_name(" display6 "), "DISPLAY6");
    }

    #[test]
    fn zone_on_connected_secondary_resolves_there() {
        assert_eq!(
            layout().zone_display_rect("notification-area"),
            Some(Rect::new(3857.0, -1079.0, 3840.0, 2160.0))
        );
    }

    #[test]
    fn unassigned_disconnected_or_primary_zones_stay_on_primary() {
        let layout = layout();
        assert_eq!(layout.zone_display_rect("pip"), None, "unassigned");
        assert_eq!(layout.zone_display_rect("subtitle"), None, "not connected");
        assert_eq!(layout.zone_display_rect("status-bar"), None, "primary");
    }

    #[test]
    fn primary_target_maps_canvas_ndc_unchanged() {
        let affine = FrameTarget::primary(2560, 1440).ndc_affine(2560, 1440);
        for p in [[-1.0, 1.0], [0.25, -0.5], [1.0, -1.0]] {
            assert_eq!(affine.apply(p), p);
        }
    }

    #[test]
    fn offset_target_maps_its_own_corners_to_ndc_corners() {
        // Canvas 2000x1000; target 1000x500 at scene (-1000, -500).
        let target = FrameTarget {
            x: -1000.0,
            y: -500.0,
            width: 1000,
            height: 500,
            primary: false,
        };
        let affine = target.ndc_affine(2000, 1000);
        // Canvas NDC of scene point (px, py) at canvas size 2000x1000.
        let ndc = |px: f32, py: f32| [px / 2000.0 * 2.0 - 1.0, 1.0 - py / 1000.0 * 2.0];
        let close =
            |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1e-5 && (a[1] - b[1]).abs() < 1e-5;
        assert!(close(affine.apply(ndc(-1000.0, -500.0)), [-1.0, 1.0]));
        assert!(close(affine.apply(ndc(0.0, 0.0)), [1.0, -1.0]));
        assert!(close(affine.apply(ndc(-500.0, -250.0)), [0.0, 0.0]));
    }
}
