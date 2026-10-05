//! One overlay window per monitor (hud-1pt5h).
//!
//! The primary window covers the primary monitor and is the scene's
//! `display_area`. Every other connected monitor gets a secondary overlay
//! window showing the same scene through its own [`FrameTarget`]: scene
//! coordinates are physical desktop pixels relative to the primary monitor's
//! top-left, so a secondary at desktop `(x, y)` sees the scene from
//! `(x - primary.x, y - primary.y)`. Zones move to a secondary only when
//! `[displays.<NAME>]` assigns them there.
//!
//! Monitor changes arrive as `WM_DISPLAYCHANGE` (a hidden-window watcher on
//! Windows) or as move/resize/DPI events on one of our windows; both mark the
//! layout dirty and the next main-loop turn re-enumerates and diffs it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tze_hud_compositor::{DisplayLayout, DisplayRect, FrameTarget, WindowSurface};
use tze_hud_scene::Rect;
use winit::event_loop::ActiveEventLoop;
use winit::monitor::MonitorHandle;
use winit::window::{Window, WindowAttributes, WindowId, WindowLevel};

/// A connected monitor in physical desktop pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MonitorSpec {
    /// Normalized OS name (`DISPLAY6`); see [`tze_hud_compositor::normalize_display_name`].
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl MonitorSpec {
    fn from_handle(index: usize, monitor: &MonitorHandle) -> Option<Self> {
        let size = monitor.size();
        if size.width == 0 || size.height == 0 {
            return None;
        }
        let position = monitor.position();
        let name = monitor
            .name()
            .map(|n| tze_hud_compositor::normalize_display_name(&n))
            .unwrap_or_else(|| format!("MONITOR{index}"));
        Some(Self {
            name,
            x: position.x,
            y: position.y,
            width: size.width,
            height: size.height,
        })
    }

    /// This monitor's bounds in scene pixels, given the primary's origin.
    pub fn scene_origin(&self, primary: &MonitorSpec) -> (i32, i32) {
        (self.x - primary.x, self.y - primary.y)
    }

    pub fn scene_rect(&self, primary: &MonitorSpec) -> Rect {
        let (x, y) = self.scene_origin(primary);
        Rect::new(x as f32, y as f32, self.width as f32, self.height as f32)
    }
}

/// The connected monitors: the primary (OS primary, else the first) and the
/// rest in enumeration order. `None` when no monitor is reported (headless).
pub(super) fn enumerate_monitors(
    event_loop: &ActiveEventLoop,
) -> Option<(MonitorSpec, Vec<MonitorSpec>)> {
    let handles: Vec<MonitorHandle> = event_loop.available_monitors().collect();
    let all: Vec<MonitorSpec> = handles
        .iter()
        .enumerate()
        .filter_map(|(i, m)| MonitorSpec::from_handle(i, m))
        .collect();
    let primary = event_loop
        .primary_monitor()
        .and_then(|p| {
            let index = handles.iter().position(|h| *h == p).unwrap_or(0);
            MonitorSpec::from_handle(index, &p)
        })
        .or_else(|| all.first().cloned())?;
    Some(split_primary(primary, all))
}

/// Separate `primary` from the full monitor list (it is enumerated too).
pub(super) fn split_primary(
    primary: MonitorSpec,
    all: Vec<MonitorSpec>,
) -> (MonitorSpec, Vec<MonitorSpec>) {
    let secondaries = all.into_iter().filter(|m| *m != primary).collect();
    (primary, secondaries)
}

/// Which current secondaries to tear down (indices, descending) and which
/// desired monitors need a new window. A monitor whose name or geometry
/// changed is torn down and recreated: simpler than moving a live surface
/// across DPI domains, and changes are rare.
pub(super) fn plan_secondary_changes(
    current: &[MonitorSpec],
    desired: &[MonitorSpec],
) -> (Vec<usize>, Vec<MonitorSpec>) {
    let mut remove: Vec<usize> = current
        .iter()
        .enumerate()
        .filter(|(_, m)| !desired.contains(m))
        .map(|(i, _)| i)
        .collect();
    remove.reverse();
    let add = desired
        .iter()
        .filter(|m| !current.contains(m))
        .cloned()
        .collect();
    (remove, add)
}

/// The compositor-facing layout: every display in scene pixels plus the
/// configured zone placement.
pub(super) fn display_layout(
    primary: &MonitorSpec,
    secondaries: &[MonitorSpec],
    zone_displays: &HashMap<String, String>,
) -> DisplayLayout {
    let mut displays = vec![DisplayRect {
        name: primary.name.clone(),
        rect: primary.scene_rect(primary),
        primary: true,
    }];
    displays.extend(secondaries.iter().map(|m| DisplayRect {
        name: m.name.clone(),
        rect: m.scene_rect(primary),
        primary: false,
    }));
    DisplayLayout::new(displays, zone_displays)
}

/// One secondary overlay window.
pub(super) struct SecondaryDisplay {
    pub spec: MonitorSpec,
    pub window: Arc<Window>,
    pub surface: Arc<WindowSurface>,
    /// Last `set_cursor_hittest` value applied to this window.
    pub capturing: bool,
}

/// What the compositor thread renders besides the primary: shared with it
/// under a mutex and re-read when `generation` changes.
#[derive(Default)]
pub(super) struct DisplayTargets {
    pub generation: u64,
    pub layout: DisplayLayout,
    /// Secondary surfaces and their scene origins.
    pub secondaries: Vec<(Arc<WindowSurface>, (i32, i32))>,
}

pub(super) type SharedDisplayTargets = Arc<StdMutex<DisplayTargets>>;

/// Compositor-thread view of one secondary.
pub(super) struct SecondaryTarget {
    pub surface: Arc<WindowSurface>,
    pub origin: (i32, i32),
    /// Signature of the last frame presented here; `None` forces a present.
    pub last_signature: Option<u64>,
}

impl SecondaryTarget {
    pub fn frame_target(&self) -> FrameTarget {
        let (width, height) = tze_hud_compositor::CompositorSurface::size(self.surface.as_ref());
        FrameTarget {
            x: self.origin.0 as f32,
            y: self.origin.1 as f32,
            width,
            height,
            primary: false,
        }
    }
}

/// Attributes for an overlay window covering `spec` (shared by the primary
/// and every secondary).
pub(super) fn overlay_window_attributes(title: String, spec: &MonitorSpec) -> WindowAttributes {
    let attrs = WindowAttributes::default()
        .with_title(title)
        .with_inner_size(winit::dpi::PhysicalSize::new(spec.width, spec.height))
        .with_position(winit::dpi::PhysicalPosition::new(spec.x, spec.y))
        .with_transparent(true)
        .with_decorations(false)
        .with_window_level(WindowLevel::AlwaysOnTop);
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::WindowAttributesExtWindows;
        // Hidden from the taskbar so the overlay cannot be minimized or
        // alt-tabbed to; WS_EX_NOREDIRECTIONBITMAP so DWM presents the
        // swapchain directly with per-pixel alpha.
        attrs
            .with_skip_taskbar(true)
            .with_no_redirection_bitmap(true)
    }
    #[cfg(not(target_os = "windows"))]
    {
        attrs
    }
}

/// Find a secondary by window id.
pub(super) fn secondary_index(secondaries: &[SecondaryDisplay], id: WindowId) -> Option<usize> {
    secondaries.iter().position(|s| s.window.id() == id)
}

/// Index of the secondary whose scene rect contains `(x, y)`.
pub(super) fn secondary_at(
    secondaries: &[SecondaryDisplay],
    primary: &MonitorSpec,
    x: f32,
    y: f32,
) -> Option<usize> {
    secondaries
        .iter()
        .position(|s| s.spec.scene_rect(primary).contains_point(x, y))
}

impl super::WinitApp {
    /// Scene origin of a window: `(0, 0)` for the primary (`None`).
    pub(super) fn window_scene_origin(&self, secondary: Option<usize>) -> (f32, f32) {
        match (secondary, self.state.primary_monitor.as_ref()) {
            (Some(i), Some(primary)) => {
                let (x, y) = self.state.secondaries[i].spec.scene_origin(primary);
                (x as f32, y as f32)
            }
            _ => (0.0, 0.0),
        }
    }

    /// Reconcile the overlay windows with the connected monitors: move the
    /// primary window if the primary monitor changed, open a window for each
    /// new monitor, close the ones whose monitor is gone, then publish the
    /// layout to the compositor thread and `/admin/status`.
    ///
    /// Only overlay auto-size runs one window per monitor; fullscreen and an
    /// explicit `--width/--height` keep the single primary window.
    pub(super) fn sync_displays(&mut self, event_loop: &ActiveEventLoop) {
        let Some(primary_window) = self.state.window.clone() else {
            return;
        };
        let multi = self.state.effective_mode == crate::window::WindowMode::Overlay
            && self.state.config.overlay_auto_size;
        let (primary, desired) = match multi.then(|| enumerate_monitors(event_loop)).flatten() {
            Some(found) => found,
            None => {
                let size = primary_window.inner_size();
                let name = primary_window
                    .current_monitor()
                    .and_then(|m| m.name())
                    .map(|n| tze_hud_compositor::normalize_display_name(&n))
                    .unwrap_or_else(|| "PRIMARY".into());
                let spec = MonitorSpec {
                    name,
                    x: 0,
                    y: 0,
                    width: size.width.max(1),
                    height: size.height.max(1),
                };
                (spec, Vec::new())
            }
        };

        let first_sync = self
            .state
            .display_targets
            .lock()
            .map_or(true, |t| t.generation == 0);
        let previous = self.state.primary_monitor.replace(primary.clone());
        let primary_changed = multi && previous.as_ref() != Some(&primary);
        if primary_changed && !first_sync {
            tracing::info!(primary = %primary.name, "primary monitor changed; moving the primary overlay");
            primary_window
                .set_outer_position(winit::dpi::PhysicalPosition::new(primary.x, primary.y));
            let _ = primary_window
                .request_inner_size(winit::dpi::PhysicalSize::new(primary.width, primary.height));
        }
        // A moved primary origin shifts every secondary's scene origin.
        let origin_moved = previous.is_some_and(|p| (p.x, p.y) != (primary.x, primary.y));
        let current: Vec<MonitorSpec> = self
            .state
            .secondaries
            .iter()
            .map(|s| s.spec.clone())
            .collect();
        let (remove, add) = if origin_moved {
            ((0..current.len()).rev().collect(), desired.clone())
        } else {
            plan_secondary_changes(&current, &desired)
        };
        if remove.is_empty() && add.is_empty() && !primary_changed && !first_sync {
            return;
        }
        for i in remove {
            let gone = self.state.secondaries.remove(i);
            tracing::info!(display = %gone.spec.name, "closing secondary overlay");
        }
        for spec in add {
            match self.create_secondary(event_loop, spec.clone()) {
                Ok(display) => {
                    tracing::info!(
                        display = %spec.name,
                        x = spec.x,
                        y = spec.y,
                        width = spec.width,
                        height = spec.height,
                        "opened secondary overlay"
                    );
                    self.state.secondaries.push(display);
                }
                Err(error) => {
                    tracing::warn!(display = %spec.name, %error, "could not open a secondary overlay");
                }
            }
        }
        // Resizes delivered while a window was being created were not routed
        // to its surface; catch the surface up to the window.
        for display in &self.state.secondaries {
            let size = display.window.inner_size();
            let current = tze_hud_compositor::CompositorSurface::size(display.surface.as_ref());
            if size.width > 0 && size.height > 0 && (size.width, size.height) != current {
                display
                    .surface
                    .pending_resize_height
                    .store(size.height, Ordering::Release);
                display
                    .surface
                    .pending_resize_width
                    .store(size.width, Ordering::Release);
            }
        }
        self.publish_displays(&primary);
    }

    fn create_secondary(
        &self,
        event_loop: &ActiveEventLoop,
        spec: MonitorSpec,
    ) -> Result<SecondaryDisplay, String> {
        let factory = self
            .state
            .surface_factory
            .as_ref()
            .ok_or("no surface factory (compositor not windowed)")?;
        let attrs = overlay_window_attributes(self.state.config.window.title.clone(), &spec);
        let window = Arc::new(event_loop.create_window(attrs).map_err(|e| e.to_string())?);
        if let Err(error) = window.set_cursor_hittest(false) {
            tracing::warn!(%error, "secondary overlay: set_cursor_hittest(false) failed");
        }
        let size = window.inner_size();
        let (width, height) = if size.width > 0 && size.height > 0 {
            (size.width, size.height)
        } else {
            (spec.width, spec.height)
        };
        let surface = factory.create(Arc::clone(&window), width, height)?;
        Ok(SecondaryDisplay {
            spec,
            window,
            surface: Arc::new(surface),
            capturing: false,
        })
    }

    /// Hand the current windows to the compositor thread and `/admin/status`.
    fn publish_displays(&mut self, primary: &MonitorSpec) {
        let specs: Vec<MonitorSpec> = self
            .state
            .secondaries
            .iter()
            .map(|s| s.spec.clone())
            .collect();
        let layout = display_layout(primary, &specs, &self.state.zone_displays);
        let zones_on = |name: &str| -> Vec<String> {
            let mut zones: Vec<String> = self
                .state
                .zone_displays
                .iter()
                .filter(|(_, d)| tze_hud_compositor::normalize_display_name(d) == name)
                .map(|(z, _)| z.clone())
                .collect();
            zones.sort();
            zones
        };
        let mut status = vec![crate::operator::status::DisplayStatus {
            name: primary.name.clone(),
            x: 0,
            y: 0,
            width: primary.width,
            height: primary.height,
            primary: true,
            zones: Vec::new(),
        }];
        status.extend(specs.iter().map(|m| {
            let (x, y) = m.scene_origin(primary);
            crate::operator::status::DisplayStatus {
                name: m.name.clone(),
                x,
                y,
                width: m.width,
                height: m.height,
                primary: false,
                zones: zones_on(&m.name),
            }
        }));
        crate::operator::status::set_displays(status);
        {
            let mut targets = self
                .state
                .display_targets
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            targets.generation += 1;
            targets.layout = layout;
            targets.secondaries = self
                .state
                .secondaries
                .iter()
                .map(|s| (Arc::clone(&s.surface), s.spec.scene_origin(primary)))
                .collect();
        }
        self.state
            .wake
            .notify_compositor(crate::idle_efficiency::RuntimeWakeupSource::Resize);
    }

    /// Present every secondary's pending frame (main thread).
    pub(super) fn present_secondaries(&self) -> bool {
        let mut presented = false;
        for display in &self.state.secondaries {
            presented |= display.surface.present_pending_texture();
        }
        presented
    }

    /// Apply the overlay capture decision per window: only the window under
    /// the cursor (or the one holding a press) captures; every other overlay
    /// stays click-through.
    pub(super) fn apply_overlay_hittest(&mut self, capture: bool) {
        let target = if self.state.left_button_down {
            self.state.press_window.and_then(|id| {
                if self.state.window.as_ref().is_some_and(|w| w.id() == id) {
                    Some(None)
                } else {
                    secondary_index(&self.state.secondaries, id).map(Some)
                }
            })
        } else {
            None
        };
        let target = target.unwrap_or_else(|| match self.state.primary_monitor.as_ref() {
            Some(primary) => secondary_at(
                &self.state.secondaries,
                primary,
                self.state.cursor_x,
                self.state.cursor_y,
            ),
            None => None,
        });
        if let Some(window) = &self.state.window {
            let primary_capture = capture && target.is_none();
            if let Err(e) = window.set_cursor_hittest(primary_capture) {
                tracing::trace!(error = %e, capture = primary_capture, "overlay: set_cursor_hittest failed");
            }
        }
        for (i, overlay) in self.state.secondaries.iter_mut().enumerate() {
            let want = capture && target == Some(i);
            if want != overlay.capturing {
                overlay.capturing = want;
                tracing::debug!(
                    name = overlay.spec.name.as_str(),
                    capture = want,
                    "overlay: secondary capture changed"
                );
                if let Err(e) = overlay.window.set_cursor_hittest(want) {
                    tracing::trace!(error = %e, capture = want, "overlay: set_cursor_hittest failed");
                }
            }
        }
    }
}

/// Flag set when the monitor layout may have changed.
pub(super) type DisplaysDirty = Arc<AtomicBool>;

pub(super) fn take_dirty(flag: &DisplaysDirty) -> bool {
    flag.swap(false, Ordering::AcqRel)
}

/// Watch for `WM_DISPLAYCHANGE` (monitor added, removed or re-moded) on a
/// hidden top-level window owned by a dedicated thread, blocked in
/// `GetMessageW` (zero idle cost). Each change sets `dirty` and calls `wake`.
#[cfg(target_os = "windows")]
pub(super) fn spawn_display_change_watcher(dirty: DisplaysDirty, wake: impl Fn() + Send + 'static) {
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_USERDATA, GetMessageW,
        GetWindowLongPtrW, MSG, RegisterClassW, SetWindowLongPtrW, WINDOW_EX_STYLE,
        WM_DISPLAYCHANGE, WNDCLASSW, WS_OVERLAPPED,
    };
    use windows::core::w;

    struct Watch {
        dirty: DisplaysDirty,
        wake: Box<dyn Fn() + Send>,
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_DISPLAYCHANGE {
            // SAFETY: GWLP_USERDATA holds the leaked `Watch` set right after
            // CreateWindowExW, or 0 before then.
            let watch = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Watch;
            if let Some(watch) = unsafe { watch.as_ref() } {
                watch.dirty.store(true, Ordering::Release);
                (watch.wake)();
            }
        }
        // SAFETY: default handling for everything else.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    let spawned = std::thread::Builder::new()
        .name("display-change-watch".into())
        .spawn(move || {
            let class = w!("tze_hud_display_watch");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                lpszClassName: class,
                ..Default::default()
            };
            // SAFETY: `wc` is fully initialised; a top-level (not message-only)
            // window is required because WM_DISPLAYCHANGE is broadcast to
            // top-level windows only. It is never shown.
            let hwnd = unsafe {
                RegisterClassW(&wc);
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    class,
                    w!(""),
                    WS_OVERLAPPED,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    None,
                    None,
                )
            };
            let hwnd = match hwnd {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    tracing::warn!(%error, "display-change watcher unavailable; monitor hotplug is noticed only via window events");
                    return;
                }
            };
            let watch = Box::into_raw(Box::new(Watch {
                dirty,
                wake: Box::new(wake),
            }));
            // SAFETY: the pointer stays valid for the thread's (process's) life.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, watch as isize) };
            let mut msg = MSG::default();
            // SAFETY: valid out-pointer; 0 (WM_QUIT) or -1 (error) ends the loop.
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
                unsafe { DispatchMessageW(&msg) };
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not start the display-change watcher");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, x: i32, y: i32) -> MonitorSpec {
        MonitorSpec {
            name: name.into(),
            x,
            y,
            width: 3840,
            height: 2160,
        }
    }

    /// The owner's rig: primary DISPLAY7 at 0,0; DISPLAY6 up-right; DISPLAY8
    /// directly above (physical = 1.5x the logical layout).
    fn rig() -> (MonitorSpec, Vec<MonitorSpec>) {
        split_primary(
            spec("DISPLAY7", 0, 0),
            vec![
                spec("DISPLAY6", 3857, -1079),
                spec("DISPLAY7", 0, 0),
                spec("DISPLAY8", 17, -2160),
            ],
        )
    }

    #[test]
    fn primary_is_split_out_of_the_enumeration() {
        let (primary, secondaries) = rig();
        assert_eq!(primary.name, "DISPLAY7");
        let names: Vec<_> = secondaries.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["DISPLAY6", "DISPLAY8"]);
    }

    #[test]
    fn scene_rects_are_relative_to_the_primary_origin_and_may_be_negative() {
        let (_, secondaries) = rig();
        let primary = spec("DISPLAY7", -100, 50);
        assert_eq!(
            secondaries[1].scene_rect(&primary),
            Rect::new(117.0, -2210.0, 3840.0, 2160.0)
        );
    }

    #[test]
    fn plan_keeps_unchanged_and_recreates_moved_monitors() {
        let current = vec![spec("DISPLAY6", 3857, -1079), spec("DISPLAY8", 17, -2160)];
        // DISPLAY8 unplugged, DISPLAY6 moved, DISPLAY9 plugged in.
        let desired = vec![spec("DISPLAY6", 3840, 0), spec("DISPLAY9", -3840, 0)];
        let (remove, add) = plan_secondary_changes(&current, &desired);
        assert_eq!(
            remove,
            vec![1, 0],
            "descending so removal keeps indices valid"
        );
        assert_eq!(add, desired);
        let (remove, add) = plan_secondary_changes(&current, &current);
        assert!(remove.is_empty() && add.is_empty(), "no change, no work");
    }

    #[test]
    fn layout_places_assigned_zones_on_their_display() {
        let (primary, secondaries) = rig();
        let zones = HashMap::from([("subtitle".to_string(), "display8".to_string())]);
        let layout = display_layout(&primary, &secondaries, &zones);
        assert_eq!(layout.displays().len(), 3);
        assert_eq!(
            layout.zone_display_rect("subtitle"),
            Some(Rect::new(17.0, -2160.0, 3840.0, 2160.0))
        );
        assert_eq!(layout.zone_display_rect("pip"), None);
    }
}
