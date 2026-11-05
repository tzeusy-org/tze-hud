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
use std::time::{Duration, Instant};

use crate::operator::status::{UnplacedReason, UnplacedZone};

use tze_hud_compositor::{DisplayLayout, DisplayRect, FrameTarget, WindowSurface};
use tze_hud_scene::Rect;
use winit::event_loop::ActiveEventLoop;
use winit::monitor::MonitorHandle;
use winit::window::{Window, WindowAttributes, WindowId, WindowLevel};

/// A connected monitor in physical desktop pixels.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct MonitorSpec {
    /// Normalized OS name (`DISPLAY6`); see [`tze_hud_compositor::normalize_display_name`].
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Windows scale factor (1.5 at 150%): physical px per logical dp
    /// before `ui.scale`.
    pub scale: f64,
}

impl MonitorSpec {
    /// Same monitor at the same place and size, whatever its scale.
    pub(super) fn same_place(&self, other: &Self) -> bool {
        (&self.name, self.x, self.y, self.width, self.height)
            == (&other.name, other.x, other.y, other.width, other.height)
    }

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
            scale: monitor.scale_factor(),
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

/// Each monitor's scale factor for appearance: the primary's, and that of
/// every zone placed on a connected secondary.
pub(super) fn display_scales(
    primary: &MonitorSpec,
    secondaries: &[MonitorSpec],
    zone_displays: &HashMap<String, String>,
) -> crate::appearance::DisplayScales {
    let zones = zone_displays
        .iter()
        .filter_map(|(zone, display)| {
            let display = tze_hud_compositor::normalize_display_name(display);
            secondaries
                .iter()
                .find(|m| m.name == display)
                .map(|m| (zone.clone(), m.scale))
        })
        .collect();
    crate::appearance::DisplayScales {
        primary: primary.scale,
        zones,
    }
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
    /// Secondary display names, surfaces and scene origins.
    pub secondaries: Vec<(String, Arc<WindowSurface>, (i32, i32))>,
    /// Secondaries whose surface the compositor gave up on (terminal loss);
    /// the main thread closes and recreates their windows. Each name is
    /// listed at most once.
    pub lost: Vec<String>,
    /// Secondaries that presented successfully since they were published;
    /// the main thread forgets their recreate count.
    pub healthy: Vec<String>,
}

impl DisplayTargets {
    /// Report `name`'s surface as lost (once, however often it is seen).
    pub fn report_lost(&mut self, name: &str) {
        if !self.lost.iter().any(|n| n == name) {
            self.lost.push(name.to_owned());
        }
    }

    /// Report that `name` presented a frame.
    pub fn report_healthy(&mut self, name: &str) {
        if !self.healthy.iter().any(|n| n == name) {
            self.healthy.push(name.to_owned());
        }
    }
}

pub(super) type SharedDisplayTargets = Arc<StdMutex<DisplayTargets>>;

/// First retry after a failed secondary present; doubles per failure.
const PRESENT_RETRY_MIN: Duration = Duration::from_millis(100);
/// Ceiling for the secondary present retry backoff.
const PRESENT_RETRY_MAX: Duration = Duration::from_secs(2);

/// Whether one secondary window shows the latest frame built for it.
///
/// A failed present (acquire timeout, occluded or asleep monitor) leaves the
/// window owed a frame, retried on a backoff deadline rather than at frame
/// cadence, so a window that keeps failing costs a few attempts a second at
/// most and never keeps the HUD rendering.
#[derive(Debug, Default)]
pub(super) struct PresentLedger {
    /// Signature of the last frame presented; `None` forces a present.
    presented: Option<u64>,
    /// Set while owed: when to retry and the backoff that produced it.
    retry: Option<(Instant, Duration)>,
    /// A present has submitted since this ledger was created.
    ever_presented: bool,
}

impl PresentLedger {
    /// A frame with `signature` differs from what is on screen.
    pub fn needs_present(&self, signature: u64) -> bool {
        self.presented != Some(signature)
    }

    /// A present may be attempted at `now` (not backing off).
    pub fn ready(&self, now: Instant) -> bool {
        self.retry.is_none_or(|(at, _)| now >= at)
    }

    /// Owed a frame and the retry time has come.
    pub fn retry_due(&self, now: Instant) -> bool {
        self.retry.is_some_and(|(at, _)| now >= at)
    }

    /// When the owed frame should be retried.
    pub fn retry_at(&self) -> Option<Instant> {
        self.retry.map(|(at, _)| at)
    }

    /// Record a present attempt of `signature` made at `now`. Returns true
    /// the first time a present submits.
    pub fn record(&mut self, signature: u64, submitted: bool, now: Instant) -> bool {
        if submitted {
            self.presented = Some(signature);
            self.retry = None;
            return !std::mem::replace(&mut self.ever_presented, true);
        }
        let backoff = self
            .retry
            .map_or(PRESENT_RETRY_MIN, |(_, b)| (b * 2).min(PRESENT_RETRY_MAX));
        self.retry = Some((now + backoff, backoff));
        false
    }

    /// The window already shows the current frame: nothing is owed.
    pub fn settle(&mut self) {
        self.retry = None;
    }

    /// Forget what is on screen (reconfigured or new surface).
    pub fn invalidate(&mut self) {
        self.presented = None;
    }

    #[cfg(test)]
    pub fn owed(&self) -> bool {
        self.retry.is_some()
    }
}

/// Compositor-thread view of one secondary.
pub(super) struct SecondaryTarget {
    pub name: String,
    pub surface: Arc<WindowSurface>,
    pub origin: (i32, i32),
    pub ledger: PresentLedger,
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

/// What one compositor iteration renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FramePlan {
    /// Build and present the primary (and every secondary that changed).
    pub full: bool,
    /// Build only to retry secondaries whose backoff ran out; the primary is
    /// neither rebuilt for hit-testing nor presented.
    pub retry_only: bool,
    /// Keep waking at frame cadence.
    pub cadence: bool,
}

/// Decide the iteration's work: `scene_needs_render` is the primary's idle
/// gate. Owed secondaries never set `cadence`; they wake on their own retry
/// deadline ([`next_secondary_retry`]).
pub(super) fn frame_plan<'a>(
    scene_needs_render: bool,
    ledgers: impl IntoIterator<Item = &'a PresentLedger>,
    now: Instant,
) -> FramePlan {
    let retry_due = !scene_needs_render && ledgers.into_iter().any(|l| l.retry_due(now));
    FramePlan {
        full: scene_needs_render,
        retry_only: retry_due,
        cadence: scene_needs_render,
    }
}

/// Floor for a retry wake that is already due but was not handled this
/// iteration (scene lock missed, or it came due mid-iteration): soon, but
/// never a spin.
const PRESENT_RETRY_FLOOR: Duration = Duration::from_millis(10);

/// When to wake for the earliest owed secondary retry.
pub(super) fn next_secondary_retry<'a>(
    ledgers: impl IntoIterator<Item = &'a PresentLedger>,
    now: Instant,
) -> Option<Instant> {
    ledgers
        .into_iter()
        .filter_map(PresentLedger::retry_at)
        .min()
        .map(|at| at.max(now + PRESENT_RETRY_FLOOR))
}

/// Present `build` to each secondary that shows something else and is not
/// backing off. Returns whether any present submitted.
pub(super) fn present_to_secondaries(
    compositor: &mut tze_hud_compositor::Compositor,
    build: &tze_hud_compositor::renderer::frame::WindowedFrameBuild,
    secondaries: &mut [SecondaryTarget],
    counters: &crate::idle_efficiency::IdleEfficiencyCounters,
    display_targets: &SharedDisplayTargets,
) -> bool {
    let mut any_submitted = false;
    for target in secondaries {
        let frame_target = target.frame_target();
        let signature = compositor.frame_signature(build, &frame_target);
        if !target.ledger.needs_present(signature) {
            // Already on screen (content returned to what was presented).
            target.ledger.settle();
            continue;
        }
        if !target.ledger.ready(Instant::now()) {
            continue;
        }
        let outcome =
            compositor.present_windowed_frame_to(build, &frame_target, target.surface.as_ref());
        if outcome.surface_acquired {
            counters.record_surface_acquisition();
        }
        if outcome.gpu_submitted {
            counters.record_gpu_submission();
        }
        let submitted = outcome.telemetry.stage7_gpu_submit_us > 0;
        if target.ledger.record(signature, submitted, Instant::now()) {
            display_targets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .report_healthy(&target.name);
        }
        any_submitted |= submitted;
    }
    any_submitted
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
        .with_window_level(WindowLevel::AlwaysOnTop)
        // Not resizable or maximizable: without WS_THICKFRAME/WS_MAXIMIZEBOX
        // Windows cannot (re-)maximize an overlay after a topology change,
        // which it did, leaving it at the monitor rect plus 11 px invisible
        // borders (-11,-11 3862x2182) and undoing every re-fit.
        .with_resizable(false)
        .with_enabled_buttons(winit::window::WindowButtons::empty())
        .with_maximized(false);
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::WindowAttributesExtWindows;
        // Hidden from the taskbar and alt-tab. Shell/display changes can
        // still minimize it; WS_EX_NOREDIRECTIONBITMAP lets DWM present the
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

/// Final swapchain outcomes: successful secondary present and deferred encodes.
pub(super) type SecondaryPresentation =
    (bool, Vec<tze_hud_compositor::surface::PresentAvailability>);

/// One of the overlay windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OverlayWindow {
    Primary,
    Secondary(usize),
}

/// Which overlay window (if any) captures the pointer. Only one window ever
/// captures; every other overlay stays click-through.
///
/// - `capture == false`: none.
/// - A held press keeps capturing on the window it started in, wherever the
///   cursor is, so the release arrives there.
/// - Otherwise the window whose monitor contains the cursor. A cursor over a
///   monitor with no overlay window captures nowhere.
pub(super) fn capture_window(
    capture: bool,
    pressed: Option<OverlayWindow>,
    cursor: (f32, f32),
    primary: &MonitorSpec,
    secondaries: &[MonitorSpec],
) -> Option<OverlayWindow> {
    if !capture {
        return None;
    }
    if pressed.is_some() {
        return pressed;
    }
    let (x, y) = cursor;
    if primary.scene_rect(primary).contains_point(x, y) {
        return Some(OverlayWindow::Primary);
    }
    secondaries
        .iter()
        .position(|m| m.scene_rect(primary).contains_point(x, y))
        .map(OverlayWindow::Secondary)
}

/// Scene coordinates of a window-relative cursor position.
pub(super) fn window_to_scene(
    position: (f64, f64),
    window: Option<&MonitorSpec>,
    primary: Option<&MonitorSpec>,
) -> (f32, f32) {
    let (ox, oy) = match (window, primary) {
        (Some(window), Some(primary)) => window.scene_origin(primary),
        _ => (0, 0),
    };
    (position.0 as f32 + ox as f32, position.1 as f32 + oy as f32)
}

/// Configured zone placements whose display is not connected:
/// `(zone, configured display)`, sorted.
pub(super) fn unplaced_zones(
    zone_displays: &HashMap<String, String>,
    connected: &[&str],
    failed: &[&str],
) -> Vec<UnplacedZone> {
    let mut out: Vec<UnplacedZone> = zone_displays
        .iter()
        .filter_map(|(zone, display)| {
            let name = tze_hud_compositor::normalize_display_name(display);
            let reason = if failed.contains(&name.as_str()) {
                UnplacedReason::OverlayFailed
            } else if connected.contains(&name.as_str()) {
                return None;
            } else {
                UnplacedReason::NotConnected
            };
            Some(UnplacedZone {
                zone: zone.clone(),
                display: display.clone(),
                reason,
            })
        })
        .collect();
    out.sort();
    out
}

/// Where an overlay window must be moved/resized to cover `spec` again, if it
/// drifted (`None` when it already covers it). `outer_position` is `None`
/// when the platform cannot report it; only the size is checked then.
pub(super) fn overlay_refit(
    outer_position: Option<(i32, i32)>,
    inner_size: (u32, u32),
    spec: &MonitorSpec,
) -> Option<((i32, i32), (u32, u32))> {
    let position = (spec.x, spec.y);
    let size = (spec.width, spec.height);
    let fitted = outer_position.is_none_or(|p| p == position) && inner_size == size;
    (!fitted).then_some((position, size))
}

/// A minimized overlay needs a no-activate restore, not another budgeted
/// resize. A matching window needs no work at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayRecovery {
    Matched,
    Restore,
    Refit((i32, i32), (u32, u32)),
}

fn overlay_recovery(
    minimized: bool,
    outer_position: Option<(i32, i32)>,
    inner_size: (u32, u32),
    spec: &MonitorSpec,
) -> OverlayRecovery {
    if minimized {
        OverlayRecovery::Restore
    } else if let Some((position, size)) = overlay_refit(outer_position, inner_size, spec) {
        OverlayRecovery::Refit(position, size)
    } else {
        OverlayRecovery::Matched
    }
}

/// Windows parks minimized windows at this sentinel. Do not mistake a real
/// monitor at that origin for a minimized window when IsIconic is false.
#[cfg(any(target_os = "windows", test))]
fn windows_overlay_minimized(
    iconic: bool,
    outer_position: Option<(i32, i32)>,
    spec: &MonitorSpec,
) -> bool {
    iconic || (outer_position == Some((-32000, -32000)) && (spec.x, spec.y) != (-32000, -32000))
}

/// Obtain the native handle on the window thread: winit intentionally refuses
/// its safe raw-handle API on the HTTP thread.
#[cfg(target_os = "windows")]
fn overlay_hwnd(window: &Window) -> Option<windows::Win32::Foundation::HWND> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return None;
    };
    Some(windows::Win32::Foundation::HWND(
        handle.hwnd.get() as *mut std::ffi::c_void
    ))
}

/// Read-only native queries may run on any thread while a strong window
/// reference keeps this HWND alive. DWM failure remains unknown.
#[cfg(target_os = "windows")]
fn native_overlay_window_state(
    hwnd: windows::Win32::Foundation::HWND,
) -> crate::operator::status::OverlayWindowState {
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongW, IsIconic, IsWindowVisible, WS_EX_TOPMOST,
    };
    let mut cloaked = 0u32;
    // SAFETY: the caller holds the window alive; the output buffer is a DWORD.
    unsafe {
        let cloak_result = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        crate::operator::status::OverlayWindowState {
            visible: IsWindowVisible(hwnd).as_bool(),
            iconic: IsIconic(hwnd).as_bool(),
            topmost: GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0,
            cloaked: cloak_result.ok().map(|()| cloaked != 0),
        }
    }
}

/// The caller owns this winit window and runs on its event-loop thread.
#[cfg(target_os = "windows")]
fn reassert_overlay_window(window: &Window, spec: Option<&MonitorSpec>) {
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_TOPMOST, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
        ShowWindow,
    };
    let Some(hwnd) = overlay_hwnd(window) else {
        tracing::warn!(window = ?window.id(), "overlay window handle unavailable");
        return;
    };
    let (x, y, width, height, flags) = if let Some(spec) = spec {
        let (Ok(width), Ok(height)) = (i32::try_from(spec.width), i32::try_from(spec.height))
        else {
            tracing::warn!(overlay = %spec.name, "overlay monitor size is not representable");
            return;
        };
        (spec.x, spec.y, width, height, SWP_NOACTIVATE)
    } else {
        (0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE)
    };
    // SAFETY: a live HWND owned by this event-loop thread. ShowWindow's BOOL
    // reports prior visibility, not success. SW_RESTORE (including winit's
    // set_minimized(false)) activates the window, so it must not be used here.
    // These undecorated, non-resizable overlays have no frame to add to the
    // monitor-native rectangle. Reassert topmost without changing focus. A
    // shell-cloaked window may remain cloaked; status keeps that fact visible.
    let result = unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, height, flags)
    };
    match result {
        Ok(()) => {
            tracing::info!(window = ?window.id(), "requested overlay visibility and topmost reassertion without activation")
        }
        Err(error) => {
            tracing::warn!(window = ?window.id(), %error, "overlay visibility reassertion failed")
        }
    }
}

/// Re-fits allowed per window per [`REFIT_WINDOW`].
const MAX_REFITS: u32 = 5;
const REFIT_WINDOW: Duration = Duration::from_secs(10);

/// Bounds re-fitting one window: if the OS keeps resizing it, give up for a
/// while instead of trading resize events with it forever.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct RefitBudget {
    window_start: Option<Instant>,
    used: u32,
}

impl RefitBudget {
    /// Only genuine geometry drift spends the refit budget. Restoring a
    /// minimized window remains possible even after the budget is exhausted.
    fn plan(&mut self, recovery: OverlayRecovery, now: Instant) -> Option<OverlayRecovery> {
        match recovery {
            OverlayRecovery::Matched => None,
            OverlayRecovery::Restore => Some(recovery),
            OverlayRecovery::Refit(..) => self.take(now).then_some(recovery),
        }
    }

    /// Spend one re-fit at `now` if the budget allows it.
    pub fn take(&mut self, now: Instant) -> bool {
        if self
            .window_start
            .is_none_or(|start| now.duration_since(start) >= REFIT_WINDOW)
        {
            self.window_start = Some(now);
            self.used = 0;
        }
        if self.used >= MAX_REFITS {
            return false;
        }
        self.used += 1;
        true
    }
}

/// Recreating a lost secondary is retried this many times per display.
const MAX_SECONDARY_RECREATES: u32 = 3;

impl super::WinitApp {
    /// Scene coordinates of a cursor position reported by a window
    /// (`secondary` = `None` for the primary).
    pub(super) fn cursor_to_scene(
        &self,
        secondary: Option<usize>,
        position: (f64, f64),
    ) -> (f32, f32) {
        window_to_scene(
            position,
            secondary.map(|i| &self.state.secondaries[i].spec),
            self.state.primary_monitor.as_ref(),
        )
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
                    scale: primary_window.scale_factor(),
                };
                (spec, Vec::new())
            }
        };

        #[cfg(target_os = "windows")]
        if self.state.effective_mode == crate::window::WindowMode::Overlay {
            for window in std::iter::once(&primary_window)
                .chain(self.state.secondaries.iter().map(|s| &s.window))
            {
                if let Some(state) = overlay_hwnd(window)
                    .map(native_overlay_window_state)
                    .filter(|s| s.needs_reassert())
                {
                    tracing::warn!(window = ?window.id(), ?state, "overlay native visibility drift");
                    reassert_overlay_window(window, None);
                }
            }
        }

        let first_sync = self
            .state
            .display_targets
            .lock()
            .map_or(true, |t| t.generation == 0);
        let previous = self.state.primary_monitor.replace(primary.clone());
        let primary_moved = multi && previous.as_ref().is_some_and(|p| !p.same_place(&primary));
        // A DPI change is a change in any mode: sizes follow the scale.
        let primary_rescaled = previous.as_ref().map(|p| p.scale) != Some(primary.scale);
        let primary_changed = primary_moved || (multi && previous.is_none()) || primary_rescaled;
        if !first_sync {
            if primary_moved {
                tracing::info!(primary = %primary.name, "primary monitor changed; moving the primary overlay");
            } else if primary_rescaled {
                tracing::info!(primary = %primary.name, scale = primary.scale, "primary monitor scale changed");
            }
        }
        // Windows whose surface the compositor lost for good: close them and
        // let the diff below recreate them, a bounded number of times. A
        // window that presented since forgets its count first, so only
        // consecutive failures disable a monitor.
        let (lost, healthy) = {
            let mut targets = self
                .state
                .display_targets
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (
                std::mem::take(&mut targets.lost),
                std::mem::take(&mut targets.healthy),
            )
        };
        for name in &healthy {
            self.state.secondary_recreates.remove(name);
        }
        // A monitor that is gone starts over if it comes back.
        self.state
            .secondary_recreates
            .retain(|name, _| desired.iter().any(|m| &m.name == name));
        let mut lost_any = false;
        for name in lost {
            let Some(i) = self
                .state
                .secondaries
                .iter()
                .position(|s| s.spec.name == name)
            else {
                // Already closed (unplugged, or reported twice).
                continue;
            };
            super::lifecycle::remove_chrome_observer(&self.state.secondaries[i].window);
            self.state.secondaries.remove(i);
            lost_any = true;
            let attempts = self
                .state
                .secondary_recreates
                .entry(name.clone())
                .or_default();
            *attempts += 1;
            if *attempts > MAX_SECONDARY_RECREATES {
                tracing::error!(overlay = %name, attempts = *attempts - 1, "secondary overlay surface keeps failing; leaving that monitor without an overlay");
            } else {
                tracing::warn!(overlay = %name, attempt = *attempts, "secondary overlay surface lost; recreating its window");
            }
        }
        let desired: Vec<MonitorSpec> = desired
            .into_iter()
            .filter(|m| !self.overlay_disabled(&m.name))
            .collect();
        // A secondary whose scale alone changed keeps its window (winit
        // follows the DPI); only its sizes are re-derived.
        let mut rescaled = false;
        for shown in &mut self.state.secondaries {
            if let Some(now) = desired
                .iter()
                .find(|d| d.same_place(&shown.spec) && d.scale != shown.spec.scale)
            {
                tracing::info!(display = %now.name, scale = now.scale, "secondary monitor scale changed");
                shown.spec.scale = now.scale;
                rescaled = true;
            }
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
        if remove.is_empty()
            && add.is_empty()
            && !primary_changed
            && !first_sync
            && !lost_any
            && !rescaled
        {
            if multi {
                self.refit_overlay_windows(&primary_window, &primary);
            }
            return;
        }
        for i in remove {
            super::lifecycle::remove_chrome_observer(&self.state.secondaries[i].window);
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
            if (size.width, size.height) != current {
                display.surface.request_resize(size.width, size.height);
            }
        }
        self.publish_displays(&primary);
        if multi {
            self.refit_overlay_windows(&primary_window, &primary);
        }
    }

    /// Consume a minimized resize before it can shrink the scene or surface.
    /// Ordinary restored resize events still use the existing resize handler.
    #[cfg(target_os = "windows")]
    pub(super) fn restore_minimized_overlay(
        &self,
        window_id: WindowId,
        secondary: Option<usize>,
        reported_size: winit::dpi::PhysicalSize<u32>,
    ) -> bool {
        if self.state.effective_mode != crate::window::WindowMode::Overlay
            || !self.state.config.overlay_auto_size
        {
            return false;
        }
        let (window, spec) = match secondary {
            Some(i) => {
                let display = &self.state.secondaries[i];
                (&display.window, &display.spec)
            }
            None => {
                let (Some(window), Some(spec)) = (&self.state.window, &self.state.primary_monitor)
                else {
                    return false;
                };
                if window.id() != window_id {
                    return false;
                }
                (window, spec)
            }
        };
        let size = window.inner_size();
        let position = window.outer_position().ok().map(|p| (p.x, p.y));
        let minimized =
            windows_overlay_minimized(window.is_minimized().unwrap_or(false), position, spec);
        match overlay_recovery(minimized, position, (size.width, size.height), spec) {
            OverlayRecovery::Restore => {
                reassert_overlay_window(window, Some(spec));
                self.state.displays_dirty.store(true, Ordering::Release);
                true
            }
            // Restoring can leave older WM_SIZE events queued. The actual
            // native rect already matches; do not apply a stale minimized
            // size to SceneGraph/config or request a surface resize for it.
            OverlayRecovery::Matched => reported_size != size,
            OverlayRecovery::Refit(..) => false,
        }
    }

    /// Pin every overlay window to its monitor's bounds. A topology or DPI
    /// change lets Windows move and resize them (seen: 237x39, and 3862x2182
    /// on a 3840x2160 monitor); nothing else would put them back.
    fn refit_overlay_windows(&mut self, primary_window: &Arc<Window>, primary: &MonitorSpec) {
        let now = Instant::now();
        let windows = std::iter::once((primary_window, primary))
            .chain(self.state.secondaries.iter().map(|s| (&s.window, &s.spec)));
        for (window, spec) in windows {
            let size = window.inner_size();
            let position = window.outer_position().ok().map(|p| (p.x, p.y));
            #[cfg(target_os = "windows")]
            let minimized =
                windows_overlay_minimized(window.is_minimized().unwrap_or(false), position, spec);
            #[cfg(not(target_os = "windows"))]
            let minimized = false;
            let recovery = overlay_recovery(minimized, position, (size.width, size.height), spec);
            if recovery == OverlayRecovery::Matched {
                continue;
            }
            let budget = self
                .state
                .overlay_refits
                .entry(spec.name.clone())
                .or_default();
            let Some(recovery) = budget.plan(recovery, now) else {
                tracing::warn!(
                    overlay = %spec.name,
                    width = size.width,
                    height = size.height,
                    "overlay window keeps being resized off its monitor; not re-fitting for now"
                );
                continue;
            };
            let OverlayRecovery::Refit((x, y), (width, height)) = recovery else {
                #[cfg(target_os = "windows")]
                reassert_overlay_window(window, Some(spec));
                continue;
            };
            tracing::info!(
                overlay = %spec.name,
                from_x = position.map(|p| p.0),
                from_y = position.map(|p| p.1),
                from_width = size.width,
                from_height = size.height,
                x,
                y,
                width,
                height,
                "re-fitting overlay window to its monitor"
            );
            // A maximized window ignores position and size requests.
            if window.is_maximized() {
                window.set_maximized(false);
            }
            window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
            let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(width, height));
        }
    }

    /// The overlay on `name` was recreated too often and is no longer tried.
    fn overlay_disabled(&self, name: &str) -> bool {
        self.state
            .secondary_recreates
            .get(name)
            .is_some_and(|n| *n > MAX_SECONDARY_RECREATES)
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
        let chrome_observer = super::lifecycle::ChromeWindowObserver::install(
            &window,
            self.state.wake.main_work_notifier(),
        );
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
        chrome_observer.publish();
        Ok(SecondaryDisplay {
            spec,
            window,
            surface: Arc::new(surface),
            capturing: false,
        })
    }

    /// Hand the current windows to the compositor thread and `/admin/status`.
    /// Tell appearance each monitor's scale, so sizes are dp on every
    /// monitor. Applied on the network runtime; the restyle arrives like any
    /// live token change.
    fn publish_display_scales(&self, primary: &MonitorSpec, secondaries: &[MonitorSpec]) {
        let (Some(appearance), Some(rt)) = (&self.state.appearance, &self.state.network_rt) else {
            return;
        };
        let scales = display_scales(primary, secondaries, &self.state.zone_displays);
        let appearance = Arc::clone(appearance);
        rt.rt.spawn(async move {
            if let Err(error) = appearance.set_display_scales(scales).await {
                tracing::warn!(%error, "appearance: display scale not applied");
            }
        });
    }

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
            zones: zones_on(&primary.name),
            window_state: None,
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
                window_state: None,
            }
        }));
        let mut connected: Vec<&str> = vec![primary.name.as_str()];
        connected.extend(specs.iter().map(|m| m.name.as_str()));
        let failed: Vec<&str> = self
            .state
            .secondary_recreates
            .keys()
            .map(String::as_str)
            .filter(|name| self.overlay_disabled(name))
            .collect();
        let unplaced = unplaced_zones(&self.state.zone_displays, &connected, &failed);
        for u in &unplaced {
            match u.reason {
                UnplacedReason::NotConnected => tracing::warn!(
                    zone = %u.zone,
                    configured = %u.display,
                    connected = ?connected,
                    "zone placed on a display that is not connected; it renders on the primary"
                ),
                UnplacedReason::OverlayFailed => tracing::warn!(
                    zone = %u.zone,
                    configured = %u.display,
                    "zone placed on a display whose overlay kept failing; it renders on the primary"
                ),
            }
        }
        #[cfg(target_os = "windows")]
        {
            // Weak references do not keep removed windows alive. Each request
            // reports the observed state before scheduling a main-thread repair.
            let windows: Vec<_> = self
                .state
                .window
                .iter()
                .map(|window| (primary.name.clone(), window))
                .chain(
                    self.state
                        .secondaries
                        .iter()
                        .map(|display| (display.spec.name.clone(), &display.window)),
                )
                .filter_map(|(name, window)| {
                    Some((
                        name,
                        Arc::downgrade(window),
                        overlay_hwnd(window)?.0 as usize,
                    ))
                })
                .collect();
            let dirty = Arc::clone(&self.state.displays_dirty);
            let wake = self.state.wake.clone();
            let overlay = self.state.effective_mode == crate::window::WindowMode::Overlay;
            crate::operator::status::set_window_state_probe(Arc::new(move || {
                let states: std::collections::HashMap<_, _> = windows
                    .iter()
                    .filter_map(|(name, window, handle)| {
                        let _window = window.upgrade()?;
                        let hwnd =
                            windows::Win32::Foundation::HWND(*handle as *mut std::ffi::c_void);
                        Some((name.clone(), native_overlay_window_state(hwnd)))
                    })
                    .collect();
                if overlay && states.values().any(|state| state.needs_reassert()) {
                    dirty.store(true, Ordering::Release);
                    wake.notify_main(crate::idle_efficiency::RuntimeWakeupSource::Resize);
                }
                states
            }));
        }
        crate::operator::status::set_displays(status, unplaced);
        self.publish_display_scales(primary, &specs);
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
                .map(|s| {
                    (
                        s.spec.name.clone(),
                        Arc::clone(&s.surface),
                        s.spec.scene_origin(primary),
                    )
                })
                .collect();
        }
        self.state
            .wake
            .notify_compositor(crate::idle_efficiency::RuntimeWakeupSource::Resize);
    }

    /// Present every secondary's pending frame (main thread).
    pub(super) fn present_secondaries(
        &self,
    ) -> (bool, Vec<tze_hud_compositor::surface::PresentAvailability>) {
        use tze_hud_compositor::surface::PendingPresent;
        let mut presented = false;
        let mut deferred = Vec::new();
        for display in &self.state.secondaries {
            match display.surface.present_pending_texture_status() {
                PendingPresent::Presented => presented = true,
                PendingPresent::Empty => {}
                PendingPresent::DeferredEncoding => {
                    deferred.push(display.surface.present_availability())
                }
            }
        }
        (presented, deferred)
    }

    /// Apply the overlay capture decision per window (see [`capture_window`]).
    pub(super) fn apply_overlay_hittest(&mut self, capture: bool) {
        let window_of = |id: WindowId| {
            if self.state.window.as_ref().is_some_and(|w| w.id() == id) {
                Some(OverlayWindow::Primary)
            } else {
                secondary_index(&self.state.secondaries, id).map(OverlayWindow::Secondary)
            }
        };
        let pressed = self
            .state
            .left_button_down
            .then_some(self.state.press_window)
            .flatten()
            .and_then(window_of);
        let target = match self.state.primary_monitor.as_ref() {
            Some(primary) => {
                let specs: Vec<MonitorSpec> = self
                    .state
                    .secondaries
                    .iter()
                    .map(|s| s.spec.clone())
                    .collect();
                capture_window(
                    capture,
                    pressed,
                    (self.state.cursor_x, self.state.cursor_y),
                    primary,
                    &specs,
                )
            }
            // No monitor model (window not created yet): primary only.
            None => capture.then_some(OverlayWindow::Primary),
        };
        if let Some(window) = &self.state.window {
            let primary_capture = target == Some(OverlayWindow::Primary);
            if let Err(e) = window.set_cursor_hittest(primary_capture) {
                tracing::trace!(error = %e, capture = primary_capture, "overlay: set_cursor_hittest failed");
            }
        }
        for (i, overlay) in self.state.secondaries.iter_mut().enumerate() {
            let want = target == Some(OverlayWindow::Secondary(i));
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

/// Watch for display changes, system resume and console display power-on on a
/// hidden top-level window owned by a dedicated thread, blocked in
/// `GetMessageW` (zero idle cost). Each change sets `dirty` and calls `wake`.
///
/// Call once per process (from the first `resumed`): the thread, its window
/// and the leaked `Watch` live for the rest of the process and are never torn
/// down; the OS reclaims them at exit.
#[cfg(target_os = "windows")]
pub(super) fn spawn_display_change_watcher(dirty: DisplaysDirty, wake: impl Fn() + Send + 'static) {
    use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Power::{
        POWERBROADCAST_SETTING, RegisterPowerSettingNotification,
        UnregisterPowerSettingNotification,
    };
    use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DispatchMessageW,
        GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, MSG, PBT_APMRESUMEAUTOMATIC,
        PBT_APMRESUMECRITICAL, PBT_APMRESUMESUSPEND, PBT_POWERSETTINGCHANGE, RegisterClassW,
        SetWindowLongPtrW, WINDOW_EX_STYLE, WM_DISPLAYCHANGE, WM_POWERBROADCAST, WNDCLASSW,
        WS_OVERLAPPED,
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
        let resumed = msg == WM_POWERBROADCAST
            && matches!(
                wparam.0 as u32,
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND | PBT_APMRESUMECRITICAL
            );
        let display_on = if msg == WM_POWERBROADCAST
            && wparam.0 as u32 == PBT_POWERSETTINGCHANGE
            && lparam.0 != 0
        {
            // SAFETY: Windows supplies this structure for PBT_POWERSETTINGCHANGE;
            // the GUID carries a DWORD. Read it unaligned from the byte payload.
            let setting = unsafe { &*(lparam.0 as *const POWERBROADCAST_SETTING) };
            setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE
                && setting.DataLength == std::mem::size_of::<u32>() as u32
                && unsafe { std::ptr::read_unaligned(setting.Data.as_ptr().cast::<u32>()) } == 1
        } else {
            false
        };
        if msg == WM_DISPLAYCHANGE || resumed || display_on {
            // SAFETY: GWLP_USERDATA holds the leaked `Watch` set right after
            // CreateWindowExW, or 0 before then.
            let watch = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Watch;
            if let Some(watch) = unsafe { watch.as_ref() } {
                watch.dirty.store(true, Ordering::Release);
                (watch.wake)();
            }
            if msg == WM_POWERBROADCAST {
                return LRESULT(1);
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
            // SAFETY: the recipient is this thread's live hidden top-level window.
            let power_notification = unsafe {
                RegisterPowerSettingNotification(HANDLE(hwnd.0), &GUID_CONSOLE_DISPLAY_STATE,
                    DEVICE_NOTIFY_WINDOW_HANDLE)
            };
            if let Err(error) = &power_notification {
                tracing::warn!(%error, "console display power notifications unavailable");
            }
            let mut msg = MSG::default();
            // SAFETY: valid out-pointer; 0 (WM_QUIT) or -1 (error) ends the loop.
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
                unsafe { DispatchMessageW(&msg) };
            }
            if let Ok(notification) = power_notification {
                // SAFETY: registration belongs to this thread and is no longer used.
                let _ = unsafe { UnregisterPowerSettingNotification(notification) };
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
            scale: 1.5,
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

    /// Zones on a connected secondary get its scale; zones on the primary,
    /// or on a monitor that is not connected, share the primary's.
    #[test]
    fn display_scales_follow_each_zones_monitor() {
        let (mut primary, mut secondaries) = rig();
        primary.scale = 1.25;
        secondaries[1].scale = 2.0;
        let zones = HashMap::from([
            ("subtitle".to_string(), r"\\.\display8".to_string()),
            ("status-bar".to_string(), "DISPLAY9".to_string()),
        ]);
        let scales = display_scales(&primary, &secondaries, &zones);
        assert_eq!(scales.primary, 1.25);
        assert_eq!(scales.zones, HashMap::from([("subtitle".to_string(), 2.0)]));
    }

    /// A DPI change alone is the same place: the window is kept.
    #[test]
    fn a_scale_change_is_not_a_move() {
        let before = spec("DISPLAY6", 3857, -1079);
        let after = MonitorSpec {
            scale: 2.0,
            ..before.clone()
        };
        assert!(before.same_place(&after));
        assert!(!before.same_place(&spec("DISPLAY6", 3840, 0)));
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

    #[test]
    fn capture_false_leaves_every_window_click_through() {
        let (primary, secondaries) = rig();
        for pressed in [None, Some(OverlayWindow::Secondary(0))] {
            assert_eq!(
                capture_window(false, pressed, (100.0, 100.0), &primary, &secondaries),
                None
            );
        }
    }

    #[test]
    fn capture_goes_to_the_window_under_the_cursor() {
        let (primary, secondaries) = rig();
        let at = |x, y| capture_window(true, None, (x, y), &primary, &secondaries);
        assert_eq!(at(100.0, 100.0), Some(OverlayWindow::Primary));
        assert_eq!(at(4000.0, -1000.0), Some(OverlayWindow::Secondary(0)));
        assert_eq!(at(100.0, -100.0), Some(OverlayWindow::Secondary(1)));
    }

    #[test]
    fn held_press_keeps_capture_on_its_window_while_cursor_is_elsewhere() {
        let (primary, secondaries) = rig();
        let pressed = Some(OverlayWindow::Secondary(0));
        assert_eq!(
            capture_window(true, pressed, (100.0, 100.0), &primary, &secondaries),
            pressed,
            "cursor over the primary, press started on DISPLAY6"
        );
    }

    #[test]
    fn cursor_over_a_monitor_without_an_overlay_captures_nowhere() {
        let (primary, secondaries) = rig();
        assert_eq!(
            capture_window(true, None, (-500.0, 500.0), &primary, &secondaries),
            None,
            "the primary must not capture input meant for another monitor"
        );
    }

    #[test]
    fn secondary_cursor_positions_map_to_scene_coordinates() {
        let (primary, secondaries) = rig();
        assert_eq!(
            window_to_scene((10.0, 20.0), Some(&secondaries[0]), Some(&primary)),
            (3867.0, -1059.0)
        );
        assert_eq!(
            window_to_scene((10.0, 20.0), None, Some(&primary)),
            (10.0, 20.0),
            "primary positions are already scene coordinates"
        );
        let offset_primary = spec("DISPLAY7", -100, 50);
        assert_eq!(
            window_to_scene((0.0, 0.0), Some(&secondaries[1]), Some(&offset_primary)),
            (117.0, -2210.0)
        );
    }

    #[test]
    fn failed_present_stays_owed_until_one_submits() {
        let t0 = Instant::now();
        let mut ledger = PresentLedger::default();
        assert!(ledger.needs_present(7), "a new window always presents");
        assert!(!ledger.record(7, false, t0));
        assert!(ledger.owed(), "an occluded/timed-out acquire owes a frame");
        assert!(ledger.needs_present(7), "the same frame is retried");
        assert!(!ledger.ready(t0), "but not before the backoff");
        assert!(ledger.ready(t0 + PRESENT_RETRY_MIN));
        assert!(
            ledger.record(7, true, t0 + PRESENT_RETRY_MIN),
            "the first successful present is reported"
        );
        assert!(!ledger.owed());
        assert!(
            !ledger.needs_present(7),
            "unchanged content does not re-present"
        );
        assert!(ledger.needs_present(8));
        assert!(
            !ledger.record(8, true, t0),
            "only the first success is reported"
        );
        ledger.invalidate();
        assert!(
            ledger.needs_present(8),
            "a reconfigured surface presents again"
        );
    }

    #[test]
    fn present_retry_backs_off_to_a_ceiling_and_resets_on_success() {
        let t0 = Instant::now();
        let mut ledger = PresentLedger::default();
        let mut now = t0;
        let mut gaps = Vec::new();
        for _ in 0..8 {
            ledger.record(1, false, now);
            let at = ledger.retry_at().expect("owed after a failure");
            gaps.push(at - now);
            now = at;
        }
        let ms: Vec<u128> = gaps.iter().map(Duration::as_millis).collect();
        assert_eq!(ms, [100, 200, 400, 800, 1600, 2000, 2000, 2000]);
        ledger.record(1, true, now);
        assert_eq!(ledger.retry_at(), None);
        ledger.record(2, false, now);
        assert_eq!(
            ledger.retry_at(),
            Some(now + PRESENT_RETRY_MIN),
            "success resets the backoff"
        );
    }

    /// N1: a secondary whose present always fails must not drive the HUD at
    /// frame cadence. Simulate the compositor loop for 10 s of an idle scene:
    /// it wakes only on the retry deadline, never at cadence, and the
    /// primary is never rebuilt.
    #[test]
    fn permanently_failing_secondary_does_not_schedule_cadence_frames() {
        let t0 = Instant::now();
        let mut ledger = PresentLedger::default();
        // The layout change that opened the window presented once and failed.
        ledger.record(1, false, t0);
        let mut now = t0;
        let mut attempts = 0;
        while now < t0 + Duration::from_secs(10) {
            let plan = frame_plan(false, [&ledger], now);
            assert!(!plan.cadence, "an owed secondary never sets frame cadence");
            assert!(
                !plan.full,
                "the primary is not rebuilt for a secondary retry"
            );
            if plan.retry_only {
                attempts += 1;
                ledger.record(1, false, now);
            }
            now = next_secondary_retry([&ledger], now).expect("the retry stays scheduled");
        }
        // Retries at 0.1, 0.3, 0.7, 1.5, 3.1, 5.1, 7.1 and 9.1 s: 8 attempts
        // in 10 s, where frame cadence would have been 600.
        assert_eq!(attempts, 8);
    }

    #[test]
    fn scene_work_keeps_cadence_and_idle_without_owed_secondaries_sleeps() {
        let now = Instant::now();
        let healthy = PresentLedger::default();
        assert_eq!(
            frame_plan(true, [&healthy], now),
            FramePlan {
                full: true,
                retry_only: false,
                cadence: true
            }
        );
        assert_eq!(
            frame_plan(false, [&healthy], now),
            FramePlan {
                full: false,
                retry_only: false,
                cadence: false
            }
        );
        assert_eq!(next_secondary_retry([&healthy], now), None);
    }

    #[test]
    fn an_overdue_retry_still_wakes_but_never_spins() {
        let t0 = Instant::now();
        let mut ledger = PresentLedger::default();
        ledger.record(1, false, t0);
        let late = t0 + Duration::from_secs(1);
        assert_eq!(
            next_secondary_retry([&ledger], late),
            Some(late + PRESENT_RETRY_FLOOR)
        );
    }

    /// The sizes Windows left the overlays at during the owner's replug.
    #[test]
    fn overlay_geometry_and_visibility_recovery() {
        let d8 = spec("DISPLAY8", 17, -2160);
        assert_eq!(overlay_refit(Some((17, -2160)), (3840, 2160), &d8), None);
        for drifted in [(3862, 2182), (237, 39)] {
            assert_eq!(
                overlay_refit(Some((17, -2160)), drifted, &d8),
                Some(((17, -2160), (3840, 2160))),
                "{drifted:?}"
            );
        }
        assert_eq!(
            overlay_refit(Some((6, -2171)), (3840, 2160), &d8),
            Some(((17, -2160), (3840, 2160))),
            "moved but the right size"
        );
        // Windows' maximized geometry from replug3.log: the monitor rect plus
        // its 11 px invisible resize borders on every side.
        let d7 = spec("DISPLAY7", 0, 0);
        assert_eq!(
            overlay_refit(Some((-11, -11)), (3862, 2182), &d7),
            Some(((0, 0), (3840, 2160)))
        );
        assert_eq!(
            overlay_refit(Some((6, -2171)), (3862, 2182), &d8),
            Some(((17, -2160), (3840, 2160)))
        );
        assert_eq!(
            overlay_refit(None, (3840, 2160), &d8),
            None,
            "no position reported: the size alone decides"
        );
        for (iconic, position, size) in [
            (true, Some((17, -2160)), (3840, 2160)),
            (true, None, (0, 0)),
            (false, Some((-32000, -32000)), (237, 39)),
        ] {
            assert_eq!(
                overlay_recovery(
                    windows_overlay_minimized(iconic, position, &d8),
                    position,
                    size,
                    &d8
                ),
                OverlayRecovery::Restore,
                "minimized windows are restored before geometry refitting"
            );
        }
        assert_eq!(
            overlay_recovery(false, Some((17, -2160)), (237, 39), &d8),
            OverlayRecovery::Refit((17, -2160), (3840, 2160)),
            "a genuine non-minimized small resize still needs a refit"
        );
        assert_eq!(
            overlay_recovery(false, None, (3840, 2160), &d8),
            OverlayRecovery::Matched
        );
        assert_eq!(
            overlay_recovery(false, Some((17, -2160)), (3840, 2160), &d8),
            OverlayRecovery::Matched,
            "the restored native rectangle needs no more work"
        );
        use crate::operator::status::OverlayWindowState;
        let healthy = OverlayWindowState {
            visible: true,
            iconic: false,
            topmost: true,
            cloaked: Some(false),
        };
        for (visible, iconic, topmost, cloaked, expected) in [
            (true, false, true, Some(false), false),
            (false, false, true, Some(false), true),
            (true, true, true, Some(false), true),
            (true, false, false, Some(false), true),
            (true, false, true, Some(true), true),
            (true, false, true, None, false),
        ] {
            let state = OverlayWindowState {
                visible,
                iconic,
                topmost,
                cloaked,
            };
            assert_eq!(state.needs_reassert(), expected, "{state:?}");
        }
        assert_eq!(
            serde_json::to_value(healthy).unwrap(),
            serde_json::json!({
                "visible": true, "iconic": false, "topmost": true, "cloaked": false,
            })
        );
        let sentinel_monitor = spec("DISPLAY9", -32000, -32000);
        assert!(!windows_overlay_minimized(
            false,
            Some((-32000, -32000)),
            &sentinel_monitor
        ));
        assert_eq!(
            overlay_recovery(
                false,
                Some((-32000, -32000)),
                (3840, 2160),
                &sentinel_monitor
            ),
            OverlayRecovery::Matched,
            "a real monitor at the sentinel origin is not minimized"
        );
    }

    #[test]
    fn refits_are_bounded_when_the_os_keeps_resizing() {
        let t0 = Instant::now();
        let mut budget = RefitBudget::default();
        for i in 0..MAX_REFITS {
            assert!(budget.take(t0 + Duration::from_millis(u64::from(i))));
        }
        assert!(!budget.take(t0 + Duration::from_secs(1)), "budget spent");
        assert!(budget.take(t0 + REFIT_WINDOW), "a new window restores it");

        let d8 = spec("DISPLAY8", 17, -2160);
        let restore = overlay_recovery(true, Some((-32000, -32000)), (237, 39), &d8);
        let refit = overlay_recovery(false, Some((17, -2160)), (237, 39), &d8);
        let matched = overlay_recovery(false, Some((17, -2160)), (3840, 2160), &d8);
        let mut budget = RefitBudget::default();
        for i in 0..MAX_REFITS {
            let now = t0 + Duration::from_millis(u64::from(i));
            assert_eq!(budget.plan(restore, now), Some(OverlayRecovery::Restore));
            assert_eq!(budget.plan(matched, now), None);
            assert_eq!(
                budget.plan(refit, now),
                Some(refit),
                "restores did not spend a refit"
            );
        }
        let exhausted = t0 + Duration::from_secs(1);
        assert_eq!(budget.plan(refit, exhausted), None);
        for _ in 0..MAX_REFITS {
            assert_eq!(
                budget.plan(restore, exhausted),
                Some(OverlayRecovery::Restore)
            );
        }
        assert_eq!(budget.plan(matched, exhausted), None);
        assert_eq!(
            budget.plan(refit, exhausted),
            None,
            "restores did not reset an exhausted budget"
        );
        assert_eq!(budget.plan(refit, t0 + REFIT_WINDOW), Some(refit));
    }

    #[test]
    fn a_lost_display_is_reported_once() {
        let mut targets = DisplayTargets::default();
        targets.report_lost("DISPLAY6");
        targets.report_lost("DISPLAY6");
        targets.report_lost("DISPLAY8");
        assert_eq!(targets.lost, ["DISPLAY6", "DISPLAY8"]);
        targets.report_healthy("DISPLAY6");
        targets.report_healthy("DISPLAY6");
        assert_eq!(targets.healthy, ["DISPLAY6"]);
    }

    #[test]
    fn zones_on_disconnected_or_failed_displays_are_reported_unplaced() {
        let zones = HashMap::from([
            ("subtitle".to_string(), r"\\.\display9".to_string()),
            ("pip".to_string(), "DISPLAY6".to_string()),
            ("ticker".to_string(), "DISPLAY7".to_string()),
        ]);
        let unplaced = |zone: &str, display: &str, reason| UnplacedZone {
            zone: zone.into(),
            display: display.into(),
            reason,
        };
        assert_eq!(
            unplaced_zones(&zones, &["DISPLAY7", "DISPLAY6"], &[]),
            vec![unplaced(
                "subtitle",
                r"\\.\display9",
                UnplacedReason::NotConnected
            )]
        );
        assert_eq!(
            unplaced_zones(&zones, &["DISPLAY6"], &["DISPLAY7"]),
            vec![
                unplaced("subtitle", r"\\.\display9", UnplacedReason::NotConnected),
                unplaced("ticker", "DISPLAY7", UnplacedReason::OverlayFailed),
            ]
        );
    }
}
