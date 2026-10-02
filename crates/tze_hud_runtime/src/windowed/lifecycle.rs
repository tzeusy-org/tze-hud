use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

use tze_hud_input::{
    DragPhase, PointerEvent, PointerEventKind, PortalRect, PortalWindowTokens, hit_affordance,
};
use tze_hud_scene::HitResult;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::ZoneInteractionKind;
use tze_hud_telemetry::{
    ConstrainedProfileIdentity, EfficiencyGpuCounters, EfficiencyPacingIdentity,
    EfficiencyPacingMode, EfficiencyRendererIdentity, EfficiencyRuntimeIdentity,
    EfficiencyScenarioIdentity, EfficiencyViewport, EfficiencyWakeupCounters, EfficiencyWindowMode,
    QUIESCENT_EFFICIENCY_SCHEMA_VERSION, QUIESCENT_INTERVAL_MIN_MS, QUIESCENT_SCENARIO_NAME,
    QUIESCENT_SCENARIO_VERSION, QUIESCENT_SETTLING_MIN_MS, QuiescentEfficiencyArtifact,
    QuiescentEfficiencyValidation, QuiescentMeasurementStatus, SessionSummary,
};

use crate::channels::{InputEvent, InputEventKind, frame_ready_channel};
use crate::threads::ShutdownToken;
use crate::window::WindowMode;
use crate::window::resolve_window_mode;

use super::input_dispatch::{
    dispatch_capture_released_event, dispatch_focus_event, dispatch_pointer_event,
    dispatch_portal_geometry_event, dispatch_scroll_offset_event, enqueue_input,
    nanoseconds_since_start,
};
use super::keyboard::ComposerDeliveryContext;
use super::portal::{
    DragReleasedData, PortalResizePointerOutcome, apply_drag_handle_pointer_event,
    apply_portal_resize_pointer_event,
};
use super::{WindowedBenchmarkConfig, WindowedQuiescentEfficiencyConfig, WinitApp};

#[cfg(target_os = "windows")]
fn hwnd_for_window(window: &Window) -> Option<windows::Win32::Foundation::HWND> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = window.window_handle().ok()?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return None;
    };
    Some(HWND(handle.hwnd.get() as *mut c_void))
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ForegroundLockTimeoutAttempt {
    previous_timeout: Option<u32>,
    lock_cleared: bool,
    lock_restored: Option<bool>,
}

#[cfg(any(target_os = "windows", test))]
impl ForegroundLockTimeoutAttempt {
    fn setting_may_be_left_altered(self) -> bool {
        self.lock_cleared && self.lock_restored != Some(true)
    }
}

/// Run focus acquisition while the foreground-lock timeout is temporarily zero.
///
/// The prior value must be captured before it is changed. If the capture fails,
/// the caller keeps its normal focus path rather than risk leaving a system-wide
/// setting altered without a value to restore.
#[cfg(any(target_os = "windows", test))]
fn with_temporary_foreground_lock_timeout<T>(
    read_timeout: impl FnOnce() -> Option<u32>,
    mut set_timeout: impl FnMut(u32) -> bool,
    focus: impl FnOnce() -> T,
) -> (T, ForegroundLockTimeoutAttempt) {
    let previous_timeout = read_timeout();
    let lock_cleared = match previous_timeout {
        Some(0) => true,
        Some(_) => set_timeout(0),
        None => false,
    };
    let focus_result = focus();
    let lock_restored = match previous_timeout {
        Some(0) => Some(true),
        Some(timeout) => Some(set_timeout(timeout)),
        None => None,
    };

    (
        focus_result,
        ForegroundLockTimeoutAttempt {
            previous_timeout,
            lock_cleared,
            lock_restored,
        },
    )
}

#[cfg(target_os = "windows")]
fn read_foreground_lock_timeout() -> Option<u32> {
    use std::ffi::c_void;
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETFOREGROUNDLOCKTIMEOUT, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };

    let mut timeout = 0;
    // SAFETY: The caller is on the event-loop thread. This reads the current
    // foreground-lock timeout and has no persistent or broadcast side effect.
    unsafe {
        SystemParametersInfoW(
            SPI_GETFOREGROUNDLOCKTIMEOUT,
            0,
            Some(&mut timeout as *mut u32 as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
        .then_some(timeout)
    }
}

#[cfg(target_os = "windows")]
fn set_foreground_lock_timeout(timeout: u32) -> bool {
    use std::ffi::c_void;
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_SETFOREGROUNDLOCKTIMEOUT, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };

    // SAFETY: SPI_SETFOREGROUNDLOCKTIMEOUT reads its value from pvParam. The
    // zero flags keep this temporary change out of the user profile and avoid a
    // WM_SETTINGCHANGE broadcast.
    unsafe {
        SystemParametersInfoW(
            SPI_SETFOREGROUNDLOCKTIMEOUT,
            0,
            Some(timeout as usize as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
    }
}

pub(super) fn focus_window_for_text_input(window: &Window) {
    window.focus_window();
    window.set_ime_allowed(true);

    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::{FALSE, TRUE};
        use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
        use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        use windows::Win32::UI::WindowsAndMessaging::{
            BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
        };

        if let Some(hwnd) = hwnd_for_window(window) {
            // Acquiring OS keyboard focus for a topmost, taskbar-hidden,
            // WS_EX_NOREDIRECTIONBITMAP overlay is subject to the Windows
            // foreground-activation lock: a bare SetForegroundWindow() from a
            // process that is not already the foreground process silently fails
            // (returns FALSE and does nothing).  When it fails the overlay
            // receives mouse input (mouse is routed via SetCapture, which does
            // not require keyboard focus) but NO WindowEvent::KeyboardInput —
            // typing and focus-scoped hotkeys (Ctrl+/-) are dead even though the
            // composer is focused at the scene level (hud-dwcr7).
            //
            // The standard Win32 workaround is to temporarily attach this
            // thread's input queue to the current foreground window's thread.
            // On the reference overlay, AttachThreadInput alone was insufficient
            // when SPI_GETFOREGROUNDLOCKTIMEOUT was effectively infinite. Save
            // the timeout, temporarily clear it with zero SPI flags (therefore
            // no persisted setting or WM_SETTINGCHANGE broadcast), then restore
            // it around SetForegroundWindow + SetFocus.
            //
            // SAFETY: The HWND is owned by this winit window on the event-loop
            // thread; all calls are standard user32 focus APIs invoked on that
            // thread. AttachThreadInput is always paired (TRUE then FALSE).
            unsafe {
                let _ = BringWindowToTop(hwnd);

                let fg = GetForegroundWindow();
                let our_thread = GetCurrentThreadId();
                // Thread that owns the current foreground window (0 if none).
                let fg_thread = if fg.0.is_null() {
                    0
                } else {
                    GetWindowThreadProcessId(fg, None)
                };

                let attach_required = fg_thread != 0 && fg_thread != our_thread;
                let attached =
                    attach_required && { AttachThreadInput(our_thread, fg_thread, TRUE).as_bool() };

                let (sfw, timeout_attempt) = with_temporary_foreground_lock_timeout(
                    read_foreground_lock_timeout,
                    set_foreground_lock_timeout,
                    || {
                        let sfw = SetForegroundWindow(hwnd).as_bool();
                        // SetFocus targets the keyboard-focus window within the
                        // (now) foreground thread's input queue; needed in
                        // addition to SetForegroundWindow so WM_KEY* messages
                        // route to our HWND.
                        let _ = SetFocus(hwnd);
                        sfw
                    },
                );

                if attached {
                    let _ = AttachThreadInput(our_thread, fg_thread, FALSE);
                }

                // The scheduled-task overlay has no stdout/stderr capture, so
                // durable diagnostics are useful when this fragile Windows path
                // fails. Success remains silent to avoid per-click log noise.
                let timeout_may_be_left_altered = timeout_attempt.setting_may_be_left_altered();
                if !sfw || (attach_required && !attached) || timeout_may_be_left_altered {
                    let fg_after = GetForegroundWindow();
                    crate::diag::diag_write(&format!(
                        "FOCUS-WARN(hud-ktwsz): foreground focus repair incomplete for overlay \
                         hwnd={:?} fg_before={:?} attached={} attach_required={} \
                         prev_lock_timeout={:?} lock_cleared={} lock_restored={:?} \
                         timeout_may_be_left_altered={} set_foreground_window={} \
                         fg_after={:?} fg_after_is_ours={}",
                        hwnd.0,
                        fg.0,
                        attached,
                        attach_required,
                        timeout_attempt.previous_timeout,
                        timeout_attempt.lock_cleared,
                        timeout_attempt.lock_restored,
                        timeout_may_be_left_altered,
                        sfw,
                        fg_after.0,
                        fg_after.0 == hwnd.0,
                    ));
                }
            }
        }
    }
}

pub(super) fn begin_os_mouse_capture(window: &Window) {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::SetCapture;

        if let Some(hwnd) = hwnd_for_window(window) {
            // SAFETY: The HWND is owned by this winit window on the event-loop thread.
            unsafe {
                let _ = SetCapture(hwnd);
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
    }
}

pub(super) fn end_os_mouse_capture() {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;

        // SAFETY: Releases mouse capture for the current thread if held.
        unsafe {
            let _ = ReleaseCapture();
        }
    }
}

fn left_mouse_button_is_physically_down() -> Option<bool> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};

        // SAFETY: GetAsyncKeyState reads the current global key state.
        let state = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) };
        Some((state & 0x8000u16 as i16) != 0)
    }

    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

#[cfg(target_os = "windows")]
pub(super) fn read_windows_clipboard_text() -> Option<String> {
    use windows::Win32::Foundation::{HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};

    const CF_UNICODETEXT_FORMAT: u32 = 13;

    // SAFETY: Clipboard access is confined to the window event-loop thread.
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT_FORMAT).is_err() {
            return None;
        }
        if OpenClipboard(HWND::default()).is_err() {
            return None;
        }

        struct ClipboardGuard;
        impl Drop for ClipboardGuard {
            fn drop(&mut self) {
                // SAFETY: Balances a successful OpenClipboard call.
                unsafe {
                    let _ = CloseClipboard();
                }
            }
        }
        let _guard = ClipboardGuard;

        let handle = GetClipboardData(CF_UNICODETEXT_FORMAT).ok()?;
        let hglobal = HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return None;
        }

        let mut len = 0usize;
        let wide = ptr as *const u16;
        while *wide.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(wide, len);
        let text = String::from_utf16_lossy(slice);
        let _ = GlobalUnlock(hglobal);

        if text.is_empty() { None } else { Some(text) }
    }
}

#[cfg(not(target_os = "windows"))]
pub(super) fn read_windows_clipboard_text() -> Option<String> {
    None
}

/// Write `text` to the Windows clipboard as CF_UNICODETEXT (the composer
/// Ctrl+C / Ctrl+X copy/cut path, hud-hxhnt). No-op on empty input.
///
/// Mirrors [`read_windows_clipboard_text`]: clipboard access is confined to the
/// window event-loop thread. Best-effort — a clipboard owned by another process
/// (open failure) is silently skipped, matching the read path's failure mode
/// (the local draft edit for a cut still applies regardless).
#[cfg(target_os = "windows")]
pub(super) fn write_windows_clipboard_text(text: &str) {
    use windows::Win32::Foundation::{HANDLE, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

    const CF_UNICODETEXT_FORMAT: u32 = 13;

    if text.is_empty() {
        return;
    }

    // UTF-16, NUL-terminated (CF_UNICODETEXT requires a wide, NUL-terminated buffer).
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);

    // SAFETY: Clipboard access is confined to the window event-loop thread. The
    // HGLOBAL is allocated with GMEM_MOVEABLE and, on a successful
    // SetClipboardData, ownership transfers to the system (we must NOT free it);
    // on any failure before that transfer the guard/scope frees nothing because
    // GlobalAlloc'd movable memory is reclaimed by the process on exit and the
    // clipboard is closed via the guard. We keep the window narrow.
    unsafe {
        if OpenClipboard(HWND::default()).is_err() {
            return;
        }
        struct ClipboardGuard;
        impl Drop for ClipboardGuard {
            fn drop(&mut self) {
                // SAFETY: Balances a successful OpenClipboard call.
                unsafe {
                    let _ = CloseClipboard();
                }
            }
        }
        let _guard = ClipboardGuard;

        if EmptyClipboard().is_err() {
            return;
        }

        let bytes = std::mem::size_of_val(wide.as_slice());
        let Ok(hglobal) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
            return;
        };
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
        let _ = GlobalUnlock(hglobal);

        // On success the system takes ownership of `hglobal`; on failure it stays
        // process-owned (reclaimed at exit) — we do not double-free.
        if SetClipboardData(CF_UNICODETEXT_FORMAT, HANDLE(hglobal.0)).is_err() {
            // Ownership was NOT transferred; leave it to the process. Nothing to do.
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub(super) fn write_windows_clipboard_text(_text: &str) {}

/// Drive the drag-handle long-press state machine for a single pointer event.
///
/// Must be called while both the `SharedState` lock **and** the inner scene
/// `Mutex<SceneGraph>` are held (i.e., inside the lock block in
/// [`WinitApp::enqueue_pointer_event`]).
///
/// Returns `Some(DragReleasedData)` when a drag completes and the caller must
/// persist the new geometry after releasing the locks.  Returns `None` for all
/// other outcomes (including in-flight moves, which are written directly to
/// `scene.tiles`).
///
/// ## Hysteresis and click-focus coexistence
///
/// The state machine uses a 250 ms long-press threshold (mouse/pointer) or 1000 ms
/// (touch) before activating the drag.  A quick press-release cycle (tap/click)
/// never reaches the `Activated` phase and therefore does not interfere with the
/// click-to-focus path wired in [`WinitApp::enqueue_pointer_event`].
/// Time budget for the main-thread interactive-feedback lock acquisition
/// (an active drag move or a live resize step).
///
/// Sized at roughly one 60 fps frame: long enough to outlast the compositor's
/// per-frame scene-lock hold (it releases at the Stage-4 `drop(scene)` every
/// frame), short enough that a pathological lock holder cannot stall the UI
/// thread for more than a frame before we fall back to dropping the update.
pub(super) const INTERACTION_LOCK_BUDGET: std::time::Duration =
    std::time::Duration::from_millis(12);

/// Bounded-spin acquisition of a [`tokio::sync::Mutex`] from the synchronous
/// main (UI) thread.
///
/// The main-thread input path only ever uses `try_lock` — it must not `.await`
/// (it is not async) and must not `blocking_lock` (that panics inside an async
/// runtime context).  For interactive gestures, dropping the local feedback on
/// a single contended `try_lock` is exactly what makes a dragged window jump in
/// bursts instead of tracking the pointer, and makes a `Ctrl+`/`-` resize step
/// silently no-op — both violations of the local-feedback-first contract.
///
/// This retries `try_lock`, yielding to the scheduler so the current holder
/// (the compositor render thread or a publish handler) can finish, until it
/// succeeds or `budget` elapses.  The wait is inherently bounded because the
/// compositor releases the scene lock at the end of every frame, so under
/// normal operation acquisition succeeds within a fraction of one frame.
/// Returns `None` only when the budget elapses, in which case the caller falls
/// back to the prior drop-the-update behaviour (no regression on a true stall).
pub(super) fn spin_acquire<T>(
    mutex: &Mutex<T>,
    budget: std::time::Duration,
) -> Option<tokio::sync::MutexGuard<'_, T>> {
    if let Ok(guard) = mutex.try_lock() {
        return Some(guard);
    }
    let start = Instant::now();
    loop {
        std::thread::yield_now();
        if let Ok(guard) = mutex.try_lock() {
            return Some(guard);
        }
        if start.elapsed() >= budget {
            return None;
        }
    }
}

/// [`spin_acquire`] plus interaction-feedback miss telemetry (hud-uyhpn).
///
/// Identical acquisition semantics to [`spin_acquire`], but on a budget timeout
/// (the guaranteed-feedback gesture is about to drop a scene update) it bumps
/// `misses` and, throttled, records the drop through the best-effort `diag` log.
///
/// This is the confirmation lever for the lock-scope fix: with the compositor no
/// longer holding the scene lock across the vsync-blocking present, a live drag
/// should never time out here, so `misses` should stay at 0. It is kept a free
/// function (not a method) so it can be called from the `if`-scrutinee position
/// without extending any `self` borrow past the if-let.
pub(super) fn spin_acquire_recording<'a, T>(
    mutex: &'a Mutex<T>,
    budget: std::time::Duration,
    misses: &std::sync::atomic::AtomicU64,
) -> Option<tokio::sync::MutexGuard<'a, T>> {
    let guard = spin_acquire(mutex, budget);
    if guard.is_none() {
        let n = misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        // First miss, then every 30th (~0.5s of dropped drag at 60Hz) — noisy
        // only while genuinely broken, which is exactly the signal we want.
        if n == 1 || n % 30 == 0 {
            crate::diag::diag_write(&format!(
                "INTERACTION-FEEDBACK-MISS: guaranteed-feedback gesture dropped a scene \
                 update (spin_acquire exceeded {}ms budget) — cumulative {n} [hud-uyhpn]",
                budget.as_millis(),
            ));
        }
    }
    guard
}

/// Decide whether an initiating `PointerDown` should acquire the scene lock with
/// the bounded interactive-feedback spin ([`spin_acquire`]) instead of a one-shot
/// `try_lock`, so a drag-move or resize gesture reliably starts even while the
/// compositor or an adapter briefly holds the scene lock.
///
/// Returns true for a runtime chrome drag handle (whole-portal move) and for a
/// portal frame's resize affordance.
///
/// The resize case MUST mirror [`super::portal::apply_portal_resize_pointer_event`]'s
/// pointer-down path (hud-yno2r): the gesture is resolved from the tile UNDER THE
/// POINTER — not the focused tile — and starts for a portal *frame*: the topmost
/// hit tile that is itself scrollable (raw-tile portal) OR that spatially
/// contains a scrollable same-namespace constituent surface (the non-scrollable
/// frame of a first-class / multi-surface portal). Keying this off the focused
/// tile — as it once did — dropped the initiating Down to a one-shot `try_lock`
/// in exactly the frame-corner state resize now supports, making resize
/// intermittently inert under scene-lock contention.
///
/// It runs against the lock-free [`crate::pipeline::HitTestSnapshot`], which
/// carries no lease/group data, so it approximates `resolve_portal_group`'s
/// lease-scoped membership with same-namespace spatial containment. A rare false
/// positive only costs a bounded spin; a false negative would reintroduce the
/// dropped-feedback bug, so the check deliberately biases toward inclusion.
pub(super) fn pointer_down_starts_guaranteed_feedback_gesture(
    snapshot: &crate::pipeline::HitTestSnapshot,
    x: f32,
    y: f32,
    portal_tokens: PortalWindowTokens,
) -> bool {
    if snapshot.hit_test_drag_handle(x, y) {
        return true;
    }

    let Some(tile) = snapshot.hit_test(x, y) else {
        return false;
    };
    let is_portal_frame = tile.has_scroll_config
        || snapshot.tiles.iter().any(|other| {
            other.has_scroll_config
                && other.namespace == tile.namespace
                && super::portal::rect_contains(&tile.bounds, &other.bounds, 1.0)
        });
    if !is_portal_frame {
        return false;
    }

    let rect = PortalRect {
        x: tile.bounds.x,
        y: tile.bounds.y,
        width: tile.bounds.width,
        height: tile.bounds.height,
    };
    hit_affordance(x, y, &rect, portal_tokens.affordance_px).is_some()
}

/// Convert a pointer event's window-space `(x, y)` into coordinates local to
/// `tile_id`'s bounds (hud-etrs0 composer hit-test). Falls back to the raw
/// pointer coordinates when the tile is not currently in the scene (e.g. a
/// drag that outlives the tile it started on).
fn tile_local_pointer_xy(
    scene: &SceneGraph,
    tile_id: &tze_hud_scene::SceneId,
    pointer_event: &PointerEvent,
) -> (f32, f32) {
    match scene.tiles.get(tile_id) {
        Some(tile) => (
            pointer_event.x - tile.bounds.x,
            pointer_event.y - tile.bounds.y,
        ),
        None => (pointer_event.x, pointer_event.y),
    }
}

pub(super) const WINDOWED_BENCHMARK_AGENT: &str = "windowed-compositor-benchmark";
const WINDOWED_BENCHMARK_SCENE: &str = "composite_tiles_v1";
const OVERLAY_COMPOSITE_DELTA_TARGET_US: i64 = 500;
const WINDOWED_BENCHMARK_INPUT_X: f32 = 16.0;
const WINDOWED_BENCHMARK_INPUT_Y: f32 = 16.0;

/// No-progress timeout for the windowed benchmark watchdog (hud-gcn01).
///
/// If the compositor renders no frames (warmup or measured) within this window,
/// the watchdog emits a partial/diagnostic artifact and exits non-zero. This
/// prevents a silent infinite hang when fullscreen window redraw callbacks never
/// fire for a non-foreground window on Windows.
pub(super) const BENCHMARK_NO_PROGRESS_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

#[derive(Clone, Copy, Debug)]
pub(super) struct PendingInputLatencySample {
    input_started_at: Instant,
    local_ack_us: u64,
}

pub(super) type PendingInputLatencySamples = Arc<StdMutex<VecDeque<PendingInputLatencySample>>>;

pub(super) fn record_pending_input_latency(
    pending: &PendingInputLatencySamples,
    input_started_at: Instant,
    local_ack_us: u64,
) {
    let local_ack_us = local_ack_us.max(1);
    if let Ok(mut samples) = pending.lock() {
        samples.push_back(PendingInputLatencySample {
            input_started_at,
            local_ack_us,
        });
    }
}

pub(super) fn drain_pending_input_latency(
    pending: &PendingInputLatencySamples,
    scene_commit_at: Instant,
    frame_present_at: Instant,
) -> Option<(u64, u64, u64)> {
    let mut samples = pending.lock().ok()?;
    if samples.is_empty() {
        return None;
    }

    let mut local_ack_us = 0;
    let mut input_to_scene_commit_us = 0;
    let mut input_to_next_present_us = 0;
    while let Some(sample) = samples.pop_front() {
        local_ack_us = local_ack_us.max(sample.local_ack_us);
        input_to_scene_commit_us = input_to_scene_commit_us.max(
            scene_commit_at
                .saturating_duration_since(sample.input_started_at)
                .as_micros() as u64,
        );
        input_to_next_present_us = input_to_next_present_us.max(
            frame_present_at
                .saturating_duration_since(sample.input_started_at)
                .as_micros() as u64,
        );
    }

    Some((
        local_ack_us,
        input_to_scene_commit_us,
        input_to_next_present_us,
    ))
}

pub(super) fn seed_windowed_benchmark_scene(scene: &mut SceneGraph, width: u32, height: u32) {
    use tze_hud_scene::types::HitRegionNode;
    use tze_hud_scene::{Capability, Node, NodeData, Rect, Rgba, SceneId, SolidColorNode};

    let width = width.max(1) as f32;
    let height = height.max(1) as f32;
    scene.display_area = Rect::new(0.0, 0.0, width, height);

    let display_order = scene
        .tabs
        .values()
        .map(|tab| tab.display_order)
        .max()
        .map_or(0, |order| order.saturating_add(1));
    let Ok(tab_id) = scene.create_tab("windowed_perf", display_order) else {
        tracing::warn!("windowed benchmark: failed to create benchmark tab");
        return;
    };
    scene.active_tab = Some(tab_id);
    let lease_id = scene.grant_lease(
        WINDOWED_BENCHMARK_AGENT,
        300_000,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );
    if let Some(lease) = scene.leases.get_mut(&lease_id) {
        lease.resource_budget.max_tiles = 32;
    }

    let cols = 5usize;
    let rows = 4usize;
    let tile_w = width / 4.85;
    let tile_h = height / 3.8;
    let step_x = width / 5.65;
    let step_y = height / 5.0;

    for i in 0..(cols * rows) {
        let col = i % cols;
        let row = i / cols;
        let x = col as f32 * step_x + (row % 2) as f32 * step_x * 0.25;
        let y = row as f32 * step_y;
        let bounds = Rect::new(x, y, tile_w, tile_h);

        let Ok(tile_id) = scene.create_tile(
            tab_id,
            WINDOWED_BENCHMARK_AGENT,
            lease_id,
            bounds,
            (i + 1) as u32,
        ) else {
            continue;
        };

        let alpha = match i % 4 {
            0 => 0.58,
            1 => 0.72,
            2 => 0.86,
            _ => 1.0,
        };
        let root_id = SceneId::new();
        let node = Node {
            layout: Default::default(),
            id: root_id,
            children: vec![],
            data: NodeData::SolidColor(SolidColorNode {
                color: Rgba::new(
                    (i % cols) as f32 / cols as f32,
                    0.32 + (row as f32 / rows as f32) * 0.45,
                    1.0 - (i as f32 / (cols * rows) as f32) * 0.7,
                    alpha,
                ),
                bounds: Rect::new(0.0, 0.0, tile_w, tile_h),
                radius: Some(10.0),
            }),
        };
        if let Err(err) = scene.set_tile_root(tile_id, node) {
            tracing::warn!(?err, "windowed benchmark: failed to set tile root");
            continue;
        }
        let hit_region = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 0.0, tile_w, tile_h),
                interaction_id: format!("windowed-benchmark-input-{i}"),
                accepts_pointer: true,
                ..Default::default()
            }),
        };
        if let Err(err) = scene.add_node_to_tile(tile_id, Some(root_id), hit_region) {
            tracing::warn!(?err, "windowed benchmark: failed to add input hit region");
        }
    }
}

pub(super) struct WindowedBenchmarkRunState {
    pub(super) config: WindowedBenchmarkConfig,
    requested_mode: WindowMode,
    effective_mode: WindowMode,
    width: u32,
    height: u32,
    target_fps: u32,
    warmup_seen: u64,
    measured_seen: u64,
    measured_start: Option<Instant>,
    pub(super) summary: SessionSummary,
    /// Instant of the last recorded frame (warmup or measured).
    /// Reset by `record()`; read by `is_stalled()` to detect no-progress hangs.
    last_frame_at: Instant,
}

/// Decide whether the windowed compositor must build and present this frame.
///
/// An explicitly configured benchmark is fixed-cadence active work: every
/// requested sample must reach [`WindowedBenchmarkRunState::record`], even when
/// its deterministic scene is otherwise unchanged. Normal runtime sessions
/// remain gated on presentation-relevant changes, except that a successfully
/// recovered swapchain needs one repaint to resume display output.
pub(super) fn windowed_frame_needs_render(
    scene_changed: bool,
    geometry_changed: bool,
    animation_inflight: bool,
    composer_needs_render: bool,
    benchmark_active: bool,
    surface_repaint_pending: bool,
) -> bool {
    benchmark_active
        || scene_changed
        || geometry_changed
        || animation_inflight
        || composer_needs_render
        || surface_repaint_pending
}

/// Carry a recovered-surface repaint requirement until its first fresh submit.
///
/// A successful reconfigure can race with a temporarily held scene lock. The
/// compositor must retain the repaint requirement across that lock miss (and
/// across a failed subsequent acquire) so an unchanged scene does not strand a
/// recovered swapchain without visible content.
pub(super) const fn update_surface_repaint_pending(
    was_pending: bool,
    surface_recovered: bool,
    frame_submitted: bool,
) -> bool {
    (was_pending || surface_recovered) && !frame_submitted
}

impl WindowedBenchmarkRunState {
    pub(super) fn new(
        config: WindowedBenchmarkConfig,
        requested_mode: WindowMode,
        effective_mode: WindowMode,
        width: u32,
        height: u32,
        target_fps: u32,
    ) -> Self {
        Self {
            config,
            requested_mode,
            effective_mode,
            width,
            height,
            target_fps,
            warmup_seen: 0,
            measured_seen: 0,
            measured_start: None,
            summary: SessionSummary::new(),
            last_frame_at: Instant::now(),
        }
    }

    pub(super) fn record(&mut self, telemetry: &tze_hud_telemetry::FrameTelemetry) -> bool {
        if telemetry.stage7_gpu_submit_us == 0 {
            return false;
        }
        self.last_frame_at = Instant::now();
        if self.warmup_seen < self.config.warmup_frames {
            self.warmup_seen += 1;
            return false;
        }

        if self.measured_start.is_none() {
            self.measured_start = Some(Instant::now());
        }
        self.summary
            .record_frame(telemetry.frame_time_us, telemetry.tile_count);
        if telemetry.input_to_local_ack_us > 0 {
            self.summary
                .input_to_local_ack
                .record(telemetry.input_to_local_ack_us);
        }
        if telemetry.input_to_scene_commit_us > 0 {
            self.summary
                .input_to_scene_commit
                .record(telemetry.input_to_scene_commit_us);
        }
        if telemetry.input_to_next_present_us > 0 {
            self.summary
                .input_to_next_present
                .record(telemetry.input_to_next_present_us);
        }
        self.measured_seen += 1;
        self.measured_seen >= self.config.frames
    }

    pub(super) fn finish(mut self) -> std::io::Result<()> {
        if let Some(start) = self.measured_start {
            self.summary.elapsed_us = start.elapsed().as_micros() as u64;
        }
        self.summary.finalize();

        let frame_time = serde_json::json!({
            "p50_us": self.summary.frame_time.p50(),
            "p99_us": self.summary.frame_time.p99(),
            "p99_9_us": self.summary.frame_time.percentile(99.9),
            "peak_us": self.summary.peak_frame_time_us,
        });
        let report = serde_json::json!({
            "schema": "tze_hud.windowed_compositor_benchmark.v1",
            "scene": WINDOWED_BENCHMARK_SCENE,
            "target": {
                "overlay_composite_delta_p99_us": OVERLAY_COMPOSITE_DELTA_TARGET_US,
            },
            "requested_mode": self.requested_mode.to_string(),
            "effective_mode": self.effective_mode.to_string(),
            "window": {
                "width": self.width,
                "height": self.height,
                "target_fps": self.target_fps,
            },
            "benchmark": {
                "warmup_frames": self.config.warmup_frames,
                "measured_frames": self.config.frames,
                "recorded_frames": self.summary.total_frames,
            },
            "frame_time": frame_time,
            "summary": self.summary,
        });

        if let Some(parent) = self.config.emit_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(
            &self.config.emit_path,
            serde_json::to_vec_pretty(&report)
                .expect("windowed benchmark report JSON serialization must succeed"),
        )
    }

    /// Returns `true` when no `record()` call has landed within `timeout`.
    ///
    /// The compositor benchmark watchdog calls this every frame loop iteration.
    /// When it fires, the caller should emit a partial result and exit non-zero
    /// instead of hanging indefinitely.
    pub(super) fn is_stalled(&self, timeout: std::time::Duration) -> bool {
        self.last_frame_at.elapsed() > timeout
    }

    /// Emit a partial/diagnostic JSON artifact for a watchdog-aborted benchmark.
    ///
    /// Writes the same schema as [`finish`] but adds a `watchdog_abort` object
    /// carrying `reason`, `timeout_secs`, and frame-progress counters so harnesses
    /// can distinguish a watchdog exit from a normal completion.
    ///
    /// The caller must set `benchmark_failed` and trigger shutdown so the process
    /// exits with a non-zero code.
    pub(super) fn emit_watchdog_abort(mut self, reason: &str) -> std::io::Result<()> {
        if let Some(start) = self.measured_start {
            self.summary.elapsed_us = start.elapsed().as_micros() as u64;
        }
        self.summary.finalize();
        let report = serde_json::json!({
            "schema": "tze_hud.windowed_compositor_benchmark.v1",
            "scene": WINDOWED_BENCHMARK_SCENE,
            "watchdog_abort": {
                "reason": reason,
                "timeout_secs": BENCHMARK_NO_PROGRESS_TIMEOUT.as_secs(),
                "warmup_frames_seen": self.warmup_seen,
                "measured_frames_seen": self.measured_seen,
            },
            "requested_mode": self.requested_mode.to_string(),
            "effective_mode": self.effective_mode.to_string(),
            "window": {
                "width": self.width,
                "height": self.height,
                "target_fps": self.target_fps,
            },
            "benchmark": {
                "warmup_frames": self.config.warmup_frames,
                "measured_frames": self.config.frames,
                "recorded_frames": self.summary.total_frames,
            },
        });
        if let Some(parent) = self.config.emit_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(
            &self.config.emit_path,
            serde_json::to_vec_pretty(&report)
                .expect("windowed benchmark watchdog JSON serialization must succeed"),
        )
    }
}

/// Maximum time to wait for the first real surface presentation before the
/// bounded measurement emits a fail-closed diagnostic artifact.
pub(super) const QUIESCENT_FIRST_PRESENT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
enum QuiescentMeasurementPhase {
    AwaitingFirstPresent {
        deadline: Instant,
    },
    Settling {
        first_present_at: Instant,
        deadline: Instant,
    },
    Measuring {
        first_present_at: Instant,
        settled_at: Instant,
        baseline: crate::idle_efficiency::IdleEfficiencySnapshot,
        deadline: Instant,
    },
    Complete,
}

/// Completion information kept separate from the file write so deterministic
/// unit tests can prove the artifact is derived from a counter delta rather
/// than a fabricated JSON fixture.
#[derive(Clone, Debug)]
pub(super) struct QuiescentMeasurementCompletion {
    pub(super) artifact: QuiescentEfficiencyArtifact,
    pub(super) validation: QuiescentEfficiencyValidation,
}

/// Bounded event-driven measurement state for a real windowed compositor.
///
/// The state starts only after `maybe_present_frame` has confirmed a main-thread
/// `SurfaceTexture::present()`. Its two `WaitUntil` sampling wakes are explicitly
/// excluded from runtime work accounting; no sampling path requests a redraw or
/// wakes the compositor.
pub(super) struct WindowedQuiescentEfficiencyRunState {
    config: WindowedQuiescentEfficiencyConfig,
    effective_mode: WindowMode,
    width: u32,
    height: u32,
    adapter: tze_hud_compositor::CompositorAdapterInfo,
    phase: QuiescentMeasurementPhase,
}

impl WindowedQuiescentEfficiencyRunState {
    pub(super) fn new(
        config: WindowedQuiescentEfficiencyConfig,
        effective_mode: WindowMode,
        width: u32,
        height: u32,
        adapter: tze_hud_compositor::CompositorAdapterInfo,
        started_at: Instant,
    ) -> Self {
        Self {
            config,
            effective_mode,
            width,
            height,
            adapter,
            phase: QuiescentMeasurementPhase::AwaitingFirstPresent {
                deadline: started_at + QUIESCENT_FIRST_PRESENT_TIMEOUT,
            },
        }
    }

    /// Start settling only after an actual main-thread presentation succeeded.
    pub(super) fn record_first_present(&mut self, presented_at: Instant) {
        if matches!(
            self.phase,
            QuiescentMeasurementPhase::AwaitingFirstPresent { .. }
        ) {
            self.phase = QuiescentMeasurementPhase::Settling {
                first_present_at: presented_at,
                deadline: presented_at + Duration::from_millis(QUIESCENT_SETTLING_MIN_MS),
            };
        }
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        match self.phase {
            QuiescentMeasurementPhase::AwaitingFirstPresent { deadline }
            | QuiescentMeasurementPhase::Settling { deadline, .. }
            | QuiescentMeasurementPhase::Measuring { deadline, .. } => Some(deadline),
            QuiescentMeasurementPhase::Complete => None,
        }
    }

    pub(super) fn emit_path(&self) -> &std::path::Path {
        &self.config.emit_path
    }

    /// Advance a due sampler deadline. Returns only after a final artifact can
    /// be written. Early or unrelated event-loop turns are intentionally inert.
    pub(super) fn advance(
        &mut self,
        now: Instant,
        counters: &crate::idle_efficiency::IdleEfficiencyCounters,
    ) -> Option<QuiescentMeasurementCompletion> {
        match self.phase.clone() {
            QuiescentMeasurementPhase::AwaitingFirstPresent { deadline } if now >= deadline => {
                counters.record_excluded_sampler_wakeup();
                self.phase = QuiescentMeasurementPhase::Complete;
                Some(self.complete(
                    counters.snapshot(),
                    0,
                    0,
                    QuiescentMeasurementStatus::Invalid,
                ))
            }
            QuiescentMeasurementPhase::Settling {
                first_present_at,
                deadline,
            } if now >= deadline => {
                counters.record_excluded_sampler_wakeup();
                let baseline = counters.snapshot();
                self.phase = QuiescentMeasurementPhase::Measuring {
                    first_present_at,
                    settled_at: now,
                    baseline,
                    deadline: now + Duration::from_millis(QUIESCENT_INTERVAL_MIN_MS),
                };
                None
            }
            QuiescentMeasurementPhase::Measuring {
                first_present_at,
                settled_at,
                baseline,
                deadline,
            } if now >= deadline => {
                counters.record_excluded_sampler_wakeup();
                let current = counters.snapshot();
                self.phase = QuiescentMeasurementPhase::Complete;
                match current.delta_since(&baseline) {
                    Ok(delta) => Some(self.complete(
                        delta,
                        millis_since(first_present_at, settled_at),
                        millis_since(settled_at, now),
                        QuiescentMeasurementStatus::Complete,
                    )),
                    Err(error) => {
                        tracing::error!(%error, "quiescent measurement counters were not monotonic");
                        Some(self.complete(
                            current,
                            millis_since(first_present_at, settled_at),
                            millis_since(settled_at, now),
                            QuiescentMeasurementStatus::Invalid,
                        ))
                    }
                }
            }
            _ => None,
        }
    }

    pub(super) fn write(&self, completion: &QuiescentMeasurementCompletion) -> std::io::Result<()> {
        if let Some(parent) = self.config.emit_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &self.config.emit_path,
            serde_json::to_vec_pretty(&completion.artifact)
                .expect("quiescent efficiency artifact serialization must succeed"),
        )
    }

    fn complete(
        &self,
        delta: crate::idle_efficiency::IdleEfficiencySnapshot,
        settling_duration_ms: u64,
        interval_duration_ms: u64,
        status: QuiescentMeasurementStatus,
    ) -> QuiescentMeasurementCompletion {
        let artifact = QuiescentEfficiencyArtifact {
            schema_version: QUIESCENT_EFFICIENCY_SCHEMA_VERSION,
            scenario: EfficiencyScenarioIdentity {
                name: QUIESCENT_SCENARIO_NAME.to_string(),
                version: QUIESCENT_SCENARIO_VERSION,
            },
            runtime: EfficiencyRuntimeIdentity {
                build: self.config.build.clone(),
                window_mode: efficiency_window_mode(self.effective_mode),
            },
            pacing: EfficiencyPacingIdentity {
                mode: EfficiencyPacingMode::EventDriven,
                requested_cadence_hz: None,
            },
            renderer: EfficiencyRendererIdentity {
                backend: self.adapter.backend.clone(),
                adapter: self.adapter.name.clone(),
                software: self.adapter.device_type.eq_ignore_ascii_case("cpu"),
            },
            viewport: EfficiencyViewport {
                width: self.width,
                height: self.height,
            },
            constrained_profile: Some(observed_constrained_profile()),
            settling_duration_ms,
            interval_duration_ms,
            status,
            wakeups: EfficiencyWakeupCounters {
                combined_runtime_driven: delta.combined_runtime_wakeups(),
                main_loop: delta.main_loop_wakeups,
                compositor_loop: delta.compositor_loop_wakeups,
                sources: delta.sources,
                excluded_sampler: delta.excluded_sampler_wakeups,
                excluded_operating_system: delta.excluded_operating_system_wakeups,
            },
            gpu: EfficiencyGpuCounters {
                queue_submissions: delta.gpu_queue_submissions,
                surface_acquisitions: delta.surface_acquisitions,
                presents: delta.presents,
            },
        };
        let validation = artifact.validate(true);
        QuiescentMeasurementCompletion {
            artifact,
            validation,
        }
    }
}

fn millis_since(start: Instant, end: Instant) -> u64 {
    end.saturating_duration_since(start)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn efficiency_window_mode(mode: WindowMode) -> EfficiencyWindowMode {
    match mode {
        WindowMode::Overlay => EfficiencyWindowMode::Overlay,
        WindowMode::Fullscreen => EfficiencyWindowMode::Fullscreen,
    }
}

fn observed_constrained_profile() -> ConstrainedProfileIdentity {
    let (logical_cpu_limit, cpu_limit_enforcement) = observed_cpu_affinity();
    ConstrainedProfileIdentity {
        operating_system: std::env::consts::OS.to_string(),
        cpu_model: observed_cpu_model(),
        logical_cpu_limit,
        cpu_limit_enforcement,
        memory_limit_bytes: observed_memory_limit_bytes(),
    }
}

fn observed_cpu_model() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Some(model) = std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|contents| {
                contents.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    (key.trim() == "model name").then(|| value.trim().to_string())
                })
            })
        {
            return model;
        }
    }
    std::env::var("PROCESSOR_IDENTIFIER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| std::env::consts::ARCH.to_string())
}

#[cfg(target_os = "linux")]
fn observed_memory_limit_bytes() -> Option<u64> {
    std::fs::read_to_string("/sys/fs/cgroup/memory.max")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
}

#[cfg(not(target_os = "linux"))]
fn observed_memory_limit_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn observed_cpu_affinity() -> (u32, String) {
    let mut affinity: libc::cpu_set_t = unsafe {
        // SAFETY: `cpu_set_t` is a plain C output buffer. Zero is the required
        // initial state before `sched_getaffinity` fills it.
        std::mem::zeroed()
    };
    let result = unsafe {
        // SAFETY: `affinity` points to a valid writable `cpu_set_t` of the exact
        // size advertised to libc; pid 0 asks for the current process only.
        libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut affinity)
    };
    if result != 0 {
        return (0, "sched_getaffinity:error".to_string());
    }

    let bytes = unsafe {
        // SAFETY: `affinity` is initialized by the successful syscall above and
        // remains live for this immutable byte-level inspection.
        std::slice::from_raw_parts(
            (&affinity as *const libc::cpu_set_t).cast::<u8>(),
            std::mem::size_of::<libc::cpu_set_t>(),
        )
    };
    let cpus: Vec<usize> = bytes
        .iter()
        .enumerate()
        .flat_map(|(byte_index, byte)| {
            (0..8).filter_map(move |bit| ((byte & (1 << bit)) != 0).then_some(byte_index * 8 + bit))
        })
        .collect();
    let rendered = cpus
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    (
        cpus.len().try_into().unwrap_or(u32::MAX),
        format!("sched_getaffinity:{rendered}"),
    )
}

#[cfg(target_os = "windows")]
fn observed_cpu_affinity() -> (u32, String) {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessAffinityMask};

    let mut process_mask = 0usize;
    let mut system_mask = 0usize;
    let result = unsafe {
        // SAFETY: both masks are valid writable pointers and `GetCurrentProcess`
        // returns the pseudo-handle for this process, whose affinity we own.
        GetProcessAffinityMask(GetCurrentProcess(), &mut process_mask, &mut system_mask)
    };
    match result {
        Ok(()) => (
            process_mask.count_ones(),
            format!("GetProcessAffinityMask:0x{process_mask:x}"),
        ),
        Err(_) => (0, "GetProcessAffinityMask:error".to_string()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn observed_cpu_affinity() -> (u32, String) {
    (0, "unsupported".to_string())
}

impl WinitApp {
    /// Best-effort per-frame reconvergence of the lock-free `active_tab_mirror`
    /// from the authoritative scene (hud-dwcr7).  Non-blocking: uses `try_lock`
    /// on both the shared-state and scene mutexes and silently skips this frame
    /// if either is busy.  This is a safety net — the mirror is primarily kept
    /// fresh at the point of each active_tab change — so it must never stall the
    /// event loop.
    pub(super) fn refresh_active_tab_mirror_opportunistic(&self) {
        let Ok(state) = self.state.shared_state.try_lock() else {
            return;
        };
        let Ok(scene) = state.scene.try_lock() else {
            return;
        };
        state.refresh_active_tab_mirror(&scene);
    }

    /// Publish the active tab's current keyboard-focus owner to the compositor's
    /// chrome-layer focus-ring pass (hud-k6yvb).
    ///
    /// Maps the `FocusManager` owner to a [`FocusRingOwner`]: a focusable node
    /// (`Some(node_id)`) or a tile-level stop (`None`). `FocusOwner::None` and
    /// chrome owners publish `None`, clearing the ring. Uses the lock-free
    /// active-tab mirror so it never stalls the event loop.
    pub(super) fn push_focus_ring_owner(&self) {
        let owner = self
            .active_tab_for_keyboard_dispatch()
            .flatten()
            .map(|tab_id| {
                (
                    tab_id,
                    self.state.focus_manager.current_owner(tab_id).clone(),
                )
            });
        let ring_owner = owner.and_then(|(tab_id, focus)| match focus {
            tze_hud_input::FocusOwner::Node { tile_id, node_id } => {
                Some(tze_hud_compositor::FocusRingOwner {
                    tab_id,
                    tile_id,
                    node_id: Some(node_id),
                })
            }
            tze_hud_input::FocusOwner::Tile(tile_id) => Some(tze_hud_compositor::FocusRingOwner {
                tab_id,
                tile_id,
                node_id: None,
            }),
            tze_hud_input::FocusOwner::None | tze_hud_input::FocusOwner::ChromeElement(_) => None,
        });
        if let Ok(mut slot) = self.state.focus_ring_owner_state.lock() {
            *slot = ring_owner;
        }
    }

    /// Publish the portal tile whose bottom-right resize corner the pointer is
    /// over to the compositor's grip pass (hud-wgiys).
    ///
    /// Per-frame + latest-wins, exactly like [`WinitApp::push_focus_ring_owner`]:
    /// the compositor swaps that tile's resize-grip mark to `hover_color` and
    /// recomputes the grip geometry from the live scene, so the highlight tracks
    /// pointer moves and portal resize/drag without instrumenting every input
    /// site. `None` (pointer off the corner, or no focused portal) leaves every
    /// grip resting.
    pub(super) fn push_resize_grip_hover(&self) {
        let target = self.resize_grip_hover_target();
        if let Ok(mut slot) = self.state.resize_grip_hover_state.lock() {
            *slot = target;
        }
    }

    pub(super) fn drain_input_capture_commands(&mut self) {
        while let Ok(command) = self.state.input_capture_rx.try_recv() {
            self.state.pending_input_capture_commands.push_back(command);
        }

        while let Some(command) = self.state.pending_input_capture_commands.pop_front() {
            let Ok(state) = self.state.shared_state.try_lock() else {
                tracing::warn!("input capture command deferred: shared_state lock busy");
                self.state
                    .pending_input_capture_commands
                    .push_front(command);
                break;
            };
            let Ok(scene) = state.scene.try_lock() else {
                tracing::warn!("input capture command deferred: scene lock busy");
                self.state
                    .pending_input_capture_commands
                    .push_front(command);
                break;
            };

            match command {
                tze_hud_protocol::session::InputCaptureCommand::Request {
                    tile_id,
                    node_id,
                    device_id,
                    release_on_up,
                } => {
                    let req = tze_hud_input::CaptureRequest {
                        tile_id,
                        node_id,
                        device_id,
                    };
                    if let Some(dispatch) =
                        self.state
                            .input_processor
                            .request_capture(&req, &scene, release_on_up)
                    {
                        tracing::debug!(
                            tile_id = ?tile_id,
                            node_id = ?node_id,
                            device_id,
                            kind = ?dispatch.kind,
                            "session input capture request applied"
                        );
                    } else {
                        tracing::warn!(
                            tile_id = ?tile_id,
                            node_id = ?node_id,
                            device_id,
                            "session input capture request ignored: target not found"
                        );
                    }
                }
                tze_hud_protocol::session::InputCaptureCommand::Release { device_id } => {
                    let req = tze_hud_input::CaptureReleaseRequest { device_id };
                    if let Some(dispatch) = self.state.input_processor.release_capture(&req, &scene)
                    {
                        dispatch_capture_released_event(&self.state.input_event_tx, dispatch);
                    }
                }
            }
        }
    }

    pub(super) fn synthesize_left_release_if_physically_up(&mut self) -> bool {
        if self.state.left_button_down
            && matches!(left_mouse_button_is_physically_down(), Some(false))
        {
            self.enqueue_pointer_event(PointerEventKind::Up);
            self.state.left_button_down = false;
            end_os_mouse_capture();
            self.update_overlay_cursor_hittest();
            return true;
        }
        false
    }

    /// Map a composer-node-local pointer position to a draft byte offset
    /// (hud-etrs0 pointer caret hit-test). `tile_local_x`/`tile_local_y` are
    /// pointer coordinates already converted to the owning tile's local space
    /// (see `tile_local_pointer_xy`); this clamps them into the node's bounds
    /// before hit-testing. Returns `None` when `node_id` does not resolve to a
    /// composer `HitRegion` in `scene`.
    ///
    /// Prefers the real glyph-geometry hit-test: `byte_at_point` against the
    /// wrapped visual-row layout the compositor already publishes for
    /// ArrowUp/ArrowDown (`self.state.composer_visual_layout`, hud-21o6x).
    /// That layout is measured with the composer's content x origin inset by
    /// `portal.spacing.content_inset_px`, so the node-local x is de-inset to
    /// match before the lookup. Mirrors the arrow-key path's freshness guard
    /// (`layout.text_len == draft.text().len()`) so a layout measured for a
    /// since-edited draft is rejected rather than mis-locating the click.
    ///
    /// The node-local `local_y` is passed straight through: `byte_at_point` maps
    /// it to a visual row via the input-box geometry the compositor publishes in
    /// the same layout (bottom-anchored short box on a tall projection portal,
    /// hud-lw60x), and `node_h` serves only as the even-split fallback height
    /// when no geometry is present.
    ///
    /// Falls back to the previous linear `(local_x / node_width) *
    /// text_byte_len` proportion when there is no fresh layout — the
    /// single-line composer profile (`portal.composer.max_lines == 1`,
    /// hud-zlfi4) never publishes one, and a multi-line composer may not have
    /// one yet on the first frame after focus gain.
    ///
    /// `layout.h_scroll_px` is folded back into the de-inset screen-space x
    /// before the lookup (hud-uui70): the single-line profile's `glyph_x`
    /// table is measured in UNSCROLLED draft space
    /// (`TextRasterizer::measure_composer_single_line_layout`), but once a
    /// long draft triggers caret-follow horizontal scroll (hud-zlfi4) the
    /// on-screen glyphs are shifted left by `h_scroll_px` — the inverse of
    /// the chrome-layer caret quad's `caret_x - h_scroll_px`
    /// (`append_composer_caret_vertices`) and `composer_ime_caret_anchor`'s
    /// identical correction: draft-space x = screen-space x + `h_scroll_px`.
    /// `h_scroll_px` is always `0.0` for the multi-line profile (it wraps
    /// instead of scrolling horizontally), so this is a no-op there. No upper
    /// clamp is applied after folding in the scroll: `byte_at_x`'s
    /// nearest-glyph search already clamps its BYTE result to the row's
    /// range, so an unclamped pixel value past the scrolled draft's visible
    /// end still resolves to the correct (rightmost) glyph — a pixel-space
    /// clamp sized to the box (not the scrolled draft) would instead
    /// truncate a valid far-scrolled x back onto an earlier, wrong glyph.
    fn composer_pointer_byte_offset(
        &self,
        scene: &SceneGraph,
        node_id: tze_hud_scene::SceneId,
        tile_local_x: f32,
        tile_local_y: f32,
    ) -> Option<usize> {
        let node = scene.nodes.get(&node_id)?;
        let tze_hud_scene::NodeData::HitRegion(hr) = &node.data else {
            return None;
        };
        let node_w = hr.bounds.width.max(1.0);
        let node_h = hr.bounds.height.max(1.0);
        let local_x = (tile_local_x - hr.bounds.x).clamp(0.0, node_w);
        let local_y = (tile_local_y - hr.bounds.y).clamp(0.0, node_h);

        let draft_text_len = self
            .state
            .input_processor
            .composer_draft_snapshot()
            .map(|(text, _, _, _, _, _)| text.len());

        let layout = self
            .state
            .composer_visual_layout
            .lock()
            .ok()
            .and_then(|guard| guard.clone());
        let fresh_layout = layout
            .filter(|l| !l.lines.is_empty() && draft_text_len.is_some_and(|len| l.text_len == len));
        if let Some(layout) = fresh_layout {
            let content_inset =
                tze_hud_config::resolve_portal_tokens(&self.state.global_tokens).content_inset_px;
            let text_x = (local_x - content_inset + layout.h_scroll_px).max(0.0);
            return Some(layout.byte_at_point(text_x, local_y, node_h));
        }

        // Fallback: the pre-hud-etrs0 crude linear proportion.
        let frac = (local_x / node_w) as f64;
        Some((frac * draft_text_len.unwrap_or(0) as f64).round() as usize)
    }

    pub(super) fn inject_windowed_benchmark_input_probe(&mut self) {
        if self.state.config.benchmark.is_none() {
            return;
        }
        self.state.cursor_x = WINDOWED_BENCHMARK_INPUT_X;
        self.state.cursor_y = WINDOWED_BENCHMARK_INPUT_Y;
        self.enqueue_pointer_event(PointerEventKind::Move);
    }

    /// Enqueue a pointer event into the input ring buffer.
    ///
    /// Maps a `PointerEventKind` to the corresponding `InputEventKind` variant
    /// understood by the channel topology and compositor pipeline.
    ///
    /// In overlay mode, dynamically toggles cursor hittest based on whether the
    /// cursor is inside any active hit-region (spec §Overlay click-through, line 181):
    /// - Inside a hit-region → `set_cursor_hittest(true)` (window captures events).
    /// - Outside all hit-regions → `set_cursor_hittest(false)` (events pass through).
    pub(super) fn enqueue_pointer_event(&mut self, kind: PointerEventKind) {
        self.refresh_widget_hover_tracking();
        let x = self.state.cursor_x;
        let y = self.state.cursor_y;

        // In overlay mode, update cursor hittest based on hit-region membership.
        // This implements per-region passthrough: pointer events outside all
        // active hit-regions are passed through to the underlying desktop, while
        // events inside any hit-region are captured by the runtime.
        //
        // We toggle on every CursorMoved so the hittest tracks the cursor as it
        // moves in/out of regions continuously.
        self.update_overlay_cursor_hittest();

        self.tick_widget_hover_tracking();

        let channel_kind = match kind {
            PointerEventKind::Move => InputEventKind::PointerMove { x, y },
            PointerEventKind::Down => InputEventKind::PointerPress { x, y, button: 0 },
            PointerEventKind::Up => InputEventKind::PointerRelease { x, y, button: 0 },
        };
        let input_event = InputEvent {
            timestamp_ns: nanoseconds_since_start(),
            kind: channel_kind,
        };
        enqueue_input(&self.state.input_ring, input_event);

        // Also feed the InputProcessor for local feedback (Stage 2).
        // This happens synchronously on the main thread per spec §Stage 2.
        let input_started_at = Instant::now();
        let pointer_event = PointerEvent {
            x,
            y,
            kind,
            device_id: 0,
            timestamp: Some(input_started_at),
        };
        // Acquire the scene lock directly (without going through SharedState) so that
        // the main-thread input path does not contend with session handlers that hold
        // both the SharedState lock and the scene lock.
        //
        // `drag_released` carries the geometry payload that must be persisted to the
        // element store after the locks are released (avoids holding locks during
        // disk I/O in the `DragEventOutcome::Released` path).
        //
        // `portal_resize_outcome` carries the geometry snapshot from a pointer-driven
        // portal resize step so that `dispatch_portal_geometry_event` can be called
        // without holding the scene lock.
        let drag_released: Option<DragReleasedData>;
        let portal_resize_outcome: Option<PortalResizePointerOutcome>;
        // Flag set when a composer focus-lost transition is detected inside the
        // locked block below; consumed after the lock is released to call
        // clear_local_composer_echo() without a borrow conflict (hud-r3ax6).
        let mut composer_focus_lost = false;
        // Flag set when a pointer caret placement / drag-select mutates the
        // composer draft selection inside the locked block below; consumed
        // after the lock is released to push the updated echo to the
        // compositor (which renders caret/selection from the local echo slot,
        // not the input processor) without a borrow conflict (hud-etrs0).
        let mut composer_selection_changed = false;
        // Interactive gestures require guaranteed same-frame local feedback
        // (local-feedback-first).  For those events, acquire the locks with a
        // bounded spin instead of a single `try_lock`, so a contended scene lock
        // cannot drop the local state/bounds update.  This covers active
        // Move/Up events from already-started gestures and, narrowly, the
        // initiating PointerDown for drag handles / portal resize affordances.
        // Ordinary content PointerDown stays on the single try_lock path to
        // preserve click-to-focus latency.
        let active_gesture_needs_guaranteed_feedback = matches!(
            pointer_event.kind,
            PointerEventKind::Move | PointerEventKind::Up
        ) && (self
            .state
            .input_processor
            .drag_states
            .values()
            .any(|s| s.phase == DragPhase::Activated)
            || self
                .state
                .portal_resize_states
                .values()
                .any(|s| s.gesture_active()));
        let initiating_down_needs_guaranteed_feedback =
            pointer_event.kind == PointerEventKind::Down && {
                let snapshot = self.state.pipeline.hit_test_snapshot.load();
                let portal_part = tze_hud_config::resolve_portal_tokens(&self.state.global_tokens);
                let portal_tokens = PortalWindowTokens {
                    min_width_px: portal_part.window_min_width_px,
                    min_height_px: portal_part.window_min_height_px,
                    resize_step_px: portal_part.window_resize_step_px,
                    affordance_px: portal_part.window_resize_affordance_px,
                };
                pointer_down_starts_guaranteed_feedback_gesture(
                    &snapshot,
                    pointer_event.x,
                    pointer_event.y,
                    portal_tokens,
                )
            };
        let needs_guaranteed_feedback =
            active_gesture_needs_guaranteed_feedback || initiating_down_needs_guaranteed_feedback;
        // Inlined into the if-let scrutinee (not a named `let`) so the guard's
        // borrow of `self` is released at the end of this if-let/else — the
        // post-lock section below needs `&mut self` (drag-release persist,
        // composer-echo clear).
        // The interaction-feedback miss counter (hud-uyhpn) is bumped inside
        // `spin_acquire_recording`, which wraps `spin_acquire` and — on a budget
        // timeout during a guaranteed-feedback gesture — records the dropped scene
        // update. Kept in the `if`-scrutinee position (not a named `let`) so the
        // returned guard remains a temporary whose borrow ends with the if-let,
        // leaving `&mut self` free for the post-lock work below. This counter is
        // the confirmation lever: after the lock-scope fix it must hold at 0
        // during a live drag.
        if let Some(state) = if needs_guaranteed_feedback {
            spin_acquire_recording(
                &self.state.shared_state,
                INTERACTION_LOCK_BUDGET,
                &self.state.interaction_feedback_lock_misses,
            )
        } else {
            self.state.shared_state.try_lock().ok()
        } {
            let scene_guard = if needs_guaranteed_feedback {
                spin_acquire_recording(
                    &state.scene,
                    INTERACTION_LOCK_BUDGET,
                    &self.state.interaction_feedback_lock_misses,
                )
            } else {
                state.scene.try_lock().ok()
            };
            if let Some(mut scene) = scene_guard {
                // ── Click-to-focus (Stage 2) ─────────────────────────────────
                // Use process_with_focus on every pointer event so that a
                // pointer-down on a focusable HitRegionNode transfers keyboard
                // focus before the AgentDispatch is produced.  The returned
                // FocusTransition carries the lost/gained events and a
                // compositor ring-update hint; we log the transition below and
                // broadcast FocusGainedEvent / FocusLostEvent to agents via
                // dispatch_focus_event on the input_event_tx channel.
                // Resolve the tab that should receive focus for this pointer
                // event.  Click-to-focus is per-tab (RFC 0004 §1.1/§1.2), so the
                // authoritative tab is the one that OWNS the tile under the
                // pointer — not the global `active_tab`.  A resident gRPC /
                // projection session (e.g. a text-stream portal) creates tiles
                // into the active tab at create time, but configs whose default
                // tab carries no widgets boot with `active_tab == None`, and
                // tab activation can otherwise drift; in either case keying focus
                // off the stale/absent global `active_tab` silently drops focus
                // acquisition (hud-dwcr7).  Pointer-down on a tile therefore
                // activates that tile's tab so focus + keyboard routing (which
                // reads `active_tab`) target the surface the operator clicked.
                if pointer_event.kind == PointerEventKind::Down {
                    if let HitResult::NodeHit { tile_id, .. } | HitResult::TileHit { tile_id } =
                        scene.hit_test(pointer_event.x, pointer_event.y)
                    {
                        if let Some(hit_tab) = scene.tiles.get(&tile_id).map(|t| t.tab_id) {
                            if scene.active_tab != Some(hit_tab) {
                                // Prefer the validated switch path (emits the
                                // active-tab-changed bookkeeping); fall back to a
                                // direct assignment if the tab is the first/only
                                // one and switch_active_tab is unavailable.
                                if scene.switch_active_tab(hit_tab).is_err() {
                                    scene.active_tab = Some(hit_tab);
                                }
                                // Keep the lock-free keyboard-dispatch mirror in
                                // sync with the tab we just activated (hud-dwcr7).
                                state.refresh_active_tab_mirror(&scene);
                                tracing::debug!(
                                    tab_id = ?hit_tab,
                                    "click-to-focus: activated tab owning clicked tile"
                                );
                            }
                        }
                    }
                }

                let active_tab = scene.active_tab;
                let (result, focus_transition) = if let Some(tab_id) = active_tab {
                    self.state.input_processor.process_with_focus(
                        &pointer_event,
                        &mut scene,
                        &mut self.state.focus_manager,
                        tab_id,
                    )
                } else {
                    // No active tab AND the pointer did not land on any tile —
                    // fall back to focus-unaware processing.
                    let r = self
                        .state
                        .input_processor
                        .process(&pointer_event, &mut scene);
                    (r, None)
                };

                // Log focus transitions and broadcast FocusGainedEvent /
                // FocusLostEvent over the FOCUS_EVENTS gRPC channel.
                // Local state is already updated in focus_manager above.
                if let Some(ref transition) = focus_transition {
                    if let Some((ev, ns)) = &transition.gained {
                        tracing::debug!(
                            namespace = %ns,
                            tile_id = ?ev.tile_id,
                            node_id = ?ev.node_id,
                            source = ?ev.source,
                            "click-to-focus: focus gained"
                        );
                        // A new focus-gain clears any stale blur delivery context
                        // from a previous composer blur so it cannot leak across
                        // focus boundaries.
                        self.state.pending_blur_delivery_context = None;
                    }
                    if let Some((ev, ns)) = &transition.lost {
                        tracing::debug!(
                            namespace = %ns,
                            tile_id = ?ev.tile_id,
                            node_id = ?ev.node_id,
                            reason = ?ev.reason,
                            "click-to-focus: focus lost"
                        );
                        // Capture the composer delivery context (namespace +
                        // node_id + tile_id) while all are still known.  If this blur
                        // triggered a composer flush (InputProcessor stored a
                        // pending_flushed_batch), composer_focused_node() is now
                        // None, so composer_delivery_context() can no longer
                        // resolve the context at flush time.  By stashing it here
                        // we allow flush_composer_draft_at_settle to deliver the
                        // terminal draft batch (§4.3 flush guarantee on blur).
                        if let Some(node_id) = ev.node_id {
                            self.state.pending_blur_delivery_context =
                                Some(ComposerDeliveryContext {
                                    namespace: ns.clone(),
                                    node_id_bytes: *node_id.as_uuid().as_bytes(),
                                    tile_id: ev.tile_id,
                                });
                        }
                        // Mark that the local echo should be cleared after this
                        // borrow scope ends (cannot call clear_local_composer_echo
                        // here because we hold the shared_state lock).
                        // Cleared below after the lock is released.
                        composer_focus_lost = true;
                    }
                }
                if let Some(transition) = focus_transition {
                    dispatch_focus_event(&self.state.input_event_tx, transition);
                }

                // ── Composer pointer-selection routing (§4.1 / hud-083az, hud-etrs0) ──
                // On pointer-Down, if a composer region is focused and the hit
                // landed on that same node, position the draft cursor via a
                // real glyph-geometry hit-test (`byte_at_point`, backed by the
                // wrapped visual-row layout the compositor already publishes
                // for ArrowUp/ArrowDown — hud-21o6x), falling back to the
                // previous linear (local_x / node_width) * text_len
                // approximation when no layout is available yet (single-line
                // composer profile, or the first frame after focus). See
                // `composer_pointer_byte_offset`. While the resulting drag
                // anchor is set, PointerMove extends the selection to the byte
                // under the pointer and PointerUp ends the drag — the
                // selection itself is untouched by Up.
                if pointer_event.kind == PointerEventKind::Down {
                    if let Some(focused_node_id) =
                        self.state.input_processor.composer_focused_node()
                    {
                        if let HitResult::NodeHit {
                            node_id, tile_id, ..
                        } = &result.hit
                        {
                            if *node_id == focused_node_id {
                                let (tile_local_x, tile_local_y) =
                                    tile_local_pointer_xy(&scene, tile_id, &pointer_event);
                                if let Some(byte_offset) = self.composer_pointer_byte_offset(
                                    &scene,
                                    *node_id,
                                    tile_local_x,
                                    tile_local_y,
                                ) {
                                    self.state
                                        .input_processor
                                        .route_pointer_selection_to_composer(
                                            byte_offset,
                                            byte_offset,
                                        );
                                    self.state.composer_pointer_drag_anchor =
                                        Some((*tile_id, byte_offset));
                                    composer_selection_changed = true;
                                    tracing::debug!(
                                        byte_offset,
                                        "composer: pointer-down positioned cursor via byte-geometry hit-test"
                                    );
                                }
                            }
                        }
                    }
                } else if pointer_event.kind == PointerEventKind::Move {
                    if let Some((anchor_tile_id, anchor_byte)) =
                        self.state.composer_pointer_drag_anchor
                    {
                        if let Some(focused_node_id) =
                            self.state.input_processor.composer_focused_node()
                        {
                            let (tile_local_x, tile_local_y) =
                                tile_local_pointer_xy(&scene, &anchor_tile_id, &pointer_event);
                            if let Some(byte_offset) = self.composer_pointer_byte_offset(
                                &scene,
                                focused_node_id,
                                tile_local_x,
                                tile_local_y,
                            ) {
                                self.state
                                    .input_processor
                                    .route_pointer_selection_to_composer(anchor_byte, byte_offset);
                                composer_selection_changed = true;
                            }
                        }
                    }
                } else if pointer_event.kind == PointerEventKind::Up {
                    self.state.composer_pointer_drag_anchor = None;
                }

                // ── Zone interaction dispatch (local feedback first) ──────────
                // On pointer-up, check whether the hit landed on a compositor-
                // managed zone interaction element (e.g. a notification dismiss
                // button).  Dismiss actions are applied synchronously here so the
                // stale affordance disappears before the next rendered frame —
                // satisfying the "local feedback first" doctrine.
                //
                // Action interactions are logged for now; callback delivery to
                // agents is handled separately (see hud-ltgk.7).
                if pointer_event.kind == PointerEventKind::Up {
                    if let HitResult::ZoneInteraction {
                        ref zone_name,
                        published_at_wall_us,
                        ref publisher_namespace,
                        ref kind,
                        ..
                    } = result.hit
                    {
                        match kind {
                            ZoneInteractionKind::Dismiss => {
                                let removed = scene.dismiss_notification(
                                    zone_name,
                                    published_at_wall_us,
                                    publisher_namespace,
                                );
                                tracing::debug!(
                                    zone = %zone_name,
                                    published_at_wall_us,
                                    publisher = %publisher_namespace,
                                    removed,
                                    "zone dismiss: notification removed from scene"
                                );
                            }
                            ZoneInteractionKind::Action { callback_id } => {
                                // Delivered to the publisher via MCP `hud_input`.
                                scene.push_pending_action(tze_hud_scene::PendingAction {
                                    publisher_namespace: publisher_namespace.clone(),
                                    zone_name: zone_name.clone(),
                                    callback_id: callback_id.clone(),
                                });
                                tracing::debug!(
                                    zone = %zone_name,
                                    published_at_wall_us,
                                    publisher = %publisher_namespace,
                                    %callback_id,
                                    "zone action: callback queued for agent delivery"
                                );
                            }
                            ZoneInteractionKind::DragHandle { .. } => {
                                // Handled by the drag state machine below — not here.
                            }
                            ZoneInteractionKind::JumpToLatest { tile_id } => {
                                // Local feedback first: snap the tile's
                                // viewport back to the tail synchronously, in
                                // the same pointer-up dispatch that produced
                                // the hit — no adapter roundtrip (hud-9ci61).
                                let changed = self
                                    .state
                                    .input_processor
                                    .reset_tile_scroll_to_tail(*tile_id, &mut scene);
                                tracing::debug!(
                                    tile_id = ?tile_id,
                                    changed,
                                    "jump-to-latest: pill clicked, scroll reset to tail"
                                );
                            }
                        }
                    }
                }

                // ── Drag-to-move: long-press drag state machine ──────────────
                // Drives the per-device long-press drag recogniser.  On Down on a
                // drag handle, starts accumulating.  On Move/Up while a drag is
                // active, moves the tile (Moved) or finalised it (Released).
                //
                // Hysteresis: the 250 ms hold threshold means a quick tap (click)
                // never reaches Activated, so click-to-focus (wired above) is
                // unaffected.  Movement > 10dp during accumulation cancels the
                // long-press recognition (Cancelled).
                let display_w = self.state.config.window.width as f32;
                let display_h = self.state.config.window.height as f32;
                drag_released = apply_drag_handle_pointer_event(
                    &mut self.state.input_processor,
                    &pointer_event,
                    &result.hit,
                    &mut scene,
                    display_w,
                    display_h,
                );

                // ── Pointer-affordance portal resize (§6b.1) ─────────────────
                // Hit-test the focused portal's resize affordance strip on every
                // pointer event.  On PointerDown inside an affordance, starts the
                // drag gesture (gesture_active = true → adapter publishes blocked).
                // On PointerMove/Up while a gesture is active, updates tile bounds
                // immediately (local-first) and carries a snapshot out of the lock
                // so the caller can broadcast an ElementRepositionedEvent after
                // releasing the scene lock.
                let portal_part = tze_hud_config::resolve_portal_tokens(&self.state.global_tokens);
                let portal_tokens = PortalWindowTokens {
                    min_width_px: portal_part.window_min_width_px,
                    min_height_px: portal_part.window_min_height_px,
                    resize_step_px: portal_part.window_resize_step_px,
                    affordance_px: portal_part.window_resize_affordance_px,
                };
                portal_resize_outcome = apply_portal_resize_pointer_event(
                    &pointer_event,
                    &mut self.state.portal_resize_states,
                    active_tab,
                    &self.state.focus_manager,
                    &mut scene,
                    display_w,
                    display_h,
                    portal_tokens,
                );

                // Local feedback patch (result.local_patch) would be sent to the
                // compositor via a local-patch channel in the full pipeline. For the
                // initial windowed runtime, the compositor reads the scene state
                // directly on the next frame.
                record_pending_input_latency(
                    &self.state.pending_input_latency,
                    input_started_at,
                    result.local_ack_us,
                );
                let _ = result.local_patch;

                // ── Pointer event dispatch to subscribed portal agents ────────
                // Broadcast PointerDown / PointerMove / PointerUp to agents
                // that have subscribed to INPUT_EVENTS (requires
                // `access_input_events` capability).  Only dispatches for
                // portal-owned hit regions (NodeHit / TileHit) — the
                // AgentDispatch is only populated by the InputProcessor when a
                // named hit region matches.  Chrome / ZoneInteraction / Passthrough
                // hits produce no AgentDispatch and are silently skipped.
                //
                // primary dispatch
                if let Some(d) = result.dispatch {
                    dispatch_pointer_event(&self.state.input_event_tx, d);
                }
                // secondary dispatches (e.g. CaptureReleased following PointerUp)
                for d in result.extra_dispatches {
                    if d.kind == tze_hud_input::AgentDispatchKind::CaptureReleased {
                        // CaptureReleased is a focus/lease lifecycle event, not a pointer
                        // event.  Route it through the FOCUS_EVENTS channel so subscribed
                        // agents receive a CaptureReleasedEvent proto envelope.
                        dispatch_capture_released_event(&self.state.input_event_tx, d);
                    } else {
                        dispatch_pointer_event(&self.state.input_event_tx, d);
                    }
                }
            } else {
                drag_released = None;
                portal_resize_outcome = None;
            }
        } else {
            drag_released = None;
            portal_resize_outcome = None;
        }

        // ── Post-lock: persist geometry override after drag release ───────────
        // The drag state machine has already updated tile.bounds for live visual
        // feedback.  Here we also write the geometry_override to the element store
        // (durable) and broadcast an ElementRepositionedEvent so subscribers know
        // the tile moved.
        if let Some(released) = drag_released {
            self.persist_drag_release(released);
        }

        // ── Post-lock: broadcast geometry event after pointer-affordance resize ─
        // Tile bounds were already updated inside the lock (local-first).  We
        // broadcast an ElementRepositionedEvent here so adapter subscribers see
        // intermediate (move) and final (up) geometry changes.  GestureStarted
        // (down) events are also emitted so adapters can observe gesture
        // lifecycle.  Fire-and-forget (best-effort coalescible state-stream
        // delivery per §6b.4).
        if let Some(outcome) = portal_resize_outcome {
            // Broadcast one geometry event per constituent surface of the portal
            // — a resize scales the whole portal as a unit (hud-fb3en), so every
            // member's new bounds must reach adapters/subscribers.
            let mut member_bounds: Vec<(tze_hud_scene::SceneId, tze_hud_scene::types::Rect)> =
                Vec::with_capacity(outcome.members.len());
            // Brief read-only re-lock (hud-s62vv): resolving a bridged member's
            // projection id requires reading its declared portal-surface identity
            // from the scene (see `push_geometry_snapshot_for_tile`). Acquired once
            // for the whole broadcast loop and only the (small) identity strings
            // are extracted — never the whole `SceneGraph` — so this stays cheap
            // on the pointer-move hot path.
            let mut portal_ids: std::collections::HashMap<tze_hud_scene::SceneId, String> =
                Default::default();
            if let Some(state) = spin_acquire(&self.state.shared_state, INTERACTION_LOCK_BUDGET) {
                if let Some(scene) = spin_acquire(&state.scene, INTERACTION_LOCK_BUDGET) {
                    portal_ids = outcome
                        .members
                        .iter()
                        .filter_map(|member| {
                            scene
                                .portal_surface(member.tile_id)
                                .map(|s| (member.tile_id, s.identity.session_id.clone()))
                        })
                        .collect();
                }
            }
            for member in &outcome.members {
                dispatch_portal_geometry_event(
                    &self.state.element_repositioned_tx,
                    member.tile_id,
                    &member.snapshot,
                    outcome.display_w,
                    outcome.display_h,
                );
                // §6b.4 producer wiring (hud-npq6g): push snapshot into the
                // in-process projection authority so the drain loop consumer sees
                // live pointer-affordance geometry (same path as the hotkey
                // resize wiring above). Also resolves bridged (first-class-surface)
                // members via their declared portal-surface identity (hud-s62vv) —
                // a bridged member has no in-process tile, so the plain tile-id
                // reverse lookup alone cannot find its projection.
                self.state
                    .portal_projection_driver
                    .push_geometry_snapshot_for_tile(
                        member.tile_id,
                        member.snapshot,
                        portal_ids.get(&member.tile_id).map(String::as_str),
                    );
                let r = member.snapshot.rect;
                member_bounds.push((
                    member.tile_id,
                    tze_hud_scene::types::Rect::new(r.x, r.y, r.width, r.height),
                ));
            }
            // Durably record every member's post-resize geometry as an id-keyed
            // override (hud-8vejp) so `list_elements` reports has_user_override
            // for all members — not just the drag-release one — and the override
            // survives a restart (authoritative at the publish ingress) rather
            // than relying solely on the transient in-session lock. Only the
            // final (PointerUp) step carries `persist`; intermediate move steps
            // set it false so the disk write fires once per gesture, not per
            // pointer-move frame.
            if outcome.persist {
                self.persist_portal_member_overrides(
                    &member_bounds,
                    outcome.display_w,
                    outcome.display_h,
                );
            }
        }

        // Post-lock: clear local composer echo if composer focus was lost inside
        // the shared_state lock scope above (hud-r3ax6). A stale pointer
        // drag-select anchor from that same composer must not leak into
        // whatever region focuses next (hud-etrs0).
        if composer_focus_lost {
            self.clear_local_composer_echo();
            self.state.composer_pointer_drag_anchor = None;
        } else if composer_selection_changed {
            // A pointer caret placement or drag-select changed the draft
            // selection; publish the refreshed snapshot so the compositor
            // renders the new caret/highlight on the next frame rather than
            // leaving it stale until the next keystroke (hud-etrs0, §4.1
            // local-feedback-first).
            self.push_local_composer_echo(input_started_at);
        }

        // Update the OS resize/move cursor to reflect the affordance under the
        // pointer (hud-g5yu1).  Runs after the gesture state machines above so
        // `active_edge`/`drag_active` reflect the event we just processed; the
        // CursorIconTracker suppresses redundant winit calls on the move stream.
        self.update_portal_cursor_icon();
    }

    /// Enqueue and process a local-first scroll event.
    ///
    /// Applies the offset locally (< 4ms p99 path) and, if the tile owner is
    /// subscribed, dispatches a `ScrollOffsetChangedEvent` to the agent via the
    /// `INPUT_EVENTS` channel.
    pub(super) fn enqueue_scroll_event(&mut self, delta_x: f32, delta_y: f32) {
        self.refresh_widget_hover_tracking();
        let x = self.state.cursor_x;
        let y = self.state.cursor_y;
        self.update_overlay_cursor_hittest();

        if let Ok(state) = self.state.shared_state.try_lock()
            && let Ok(mut scene) = state.scene.try_lock()
        {
            if let Some(ev) = self.state.input_processor.process_scroll_event(
                &tze_hud_input::ScrollEvent {
                    x,
                    y,
                    delta_x,
                    delta_y,
                },
                &mut scene,
            ) {
                dispatch_scroll_offset_event(&self.state.input_event_tx, &scene, ev);
            }
        }
    }
}

impl WinitApp {
    /// Tear down the current window/compositor and apply a pending mode switch.
    ///
    /// Called from `about_to_wait` when `pending_mode_switch` is `Some`.
    /// After this returns, `self.state.window` is `None` so that `resumed()`
    /// will re-create the window with the new effective mode.
    pub(super) fn apply_pending_mode_switch(&mut self) {
        let new_mode = match self.state.pending_mode_switch.take() {
            Some(m) => m,
            None => return,
        };

        tracing::info!(
            old_mode = %self.state.effective_mode,
            new_mode = %new_mode,
            "runtime mode switch: tearing down existing window for surface recreation"
        );

        // Join the compositor thread before destroying the surface.
        if let Some(handle) = self.state.compositor_handle.take() {
            self.state
                .shutdown
                .trigger(crate::threads::ShutdownReason::Clean);
            self.state
                .wake
                .notify_compositor(crate::idle_efficiency::RuntimeWakeupSource::Shutdown);
            let _ = handle.join();
        }

        // Drop the surface and window handles.
        self.state.window_surface = None;
        self.state.window = None;

        // Re-create the shutdown token for the new session.
        self.state.shutdown = ShutdownToken::new();

        // Re-create the frame-ready channel.
        let (new_tx, new_rx) = frame_ready_channel();
        self.state.frame_ready_tx = Some(new_tx);
        self.state.frame_ready_rx = new_rx;

        // Apply the new mode (with platform fallback check).
        // resolve_window_mode() emits the fallback warning internally;
        // no duplicate logging needed here.
        let (resolved_mode, _) = resolve_window_mode(new_mode);
        self.state.effective_mode = resolved_mode;
        self.state.config.window.mode = resolved_mode;
    }

    /// Cycle the overlay to the next (+1) or previous (-1) monitor.
    ///
    /// Enumerates available monitors, advances the index, and repositions +
    /// resizes the window to cover the target monitor's full physical area.
    /// The compositor surface is reconfigured automatically via the existing
    /// `WindowEvent::Resized` handler.
    pub(super) fn cycle_monitor(&mut self, event_loop: &ActiveEventLoop, direction: i32) {
        let monitors: Vec<_> = event_loop.available_monitors().collect();
        if monitors.is_empty() {
            return;
        }
        let count = monitors.len();
        let new_idx = ((self.state.current_monitor_index as i32 + direction)
            .rem_euclid(count as i32)) as usize;
        self.state.current_monitor_index = new_idx;

        let m = &monitors[new_idx];
        let size = m.size();
        let pos = m.position();
        tracing::info!(
            monitor_index = new_idx,
            name = m.name().as_deref().unwrap_or("<unnamed>"),
            width = size.width,
            height = size.height,
            x = pos.x,
            y = pos.y,
            "monitor cycle: moving overlay"
        );

        if let Some(window) = &self.state.window {
            window.set_outer_position(winit::dpi::PhysicalPosition::new(pos.x, pos.y));
            let _ =
                window.request_inner_size(winit::dpi::PhysicalSize::new(size.width, size.height));
        }
    }

    /// Check the `FrameReadySignal` and present the frame if the compositor
    /// has signalled one.
    ///
    /// Per spec §Compositor Thread Ownership (line 54-55):
    /// "WHEN a frame is ready for presentation THEN the compositor thread MUST
    /// signal the main thread via FrameReadySignal, and only the main thread
    /// SHALL call surface.present()."
    ///
    /// The compositor thread stores the rendered `SurfaceTexture` in
    /// `WindowSurface::pending_texture` during `acquire_frame()`. This method
    /// retrieves that exact texture via `take_pending_texture()` and calls
    /// `SurfaceTexture::present()` on it — satisfying the macOS/Metal requirement
    /// that `present()` runs on the main thread, and ensuring we present the
    /// texture the compositor actually rendered into.
    pub(super) fn maybe_present_frame(&mut self) {
        if self.state.frame_ready_rx.has_changed().unwrap_or(false) {
            // Acknowledge the signal.
            let _ = self.state.frame_ready_rx.borrow_and_update();

            if let Some(surface) = &self.state.window_surface {
                // Present under the same mutex that guards pending swapchain
                // ownership so acquire/present cannot interleave into a
                // double-acquire validation error on some backends.
                if surface.present_pending_texture() {
                    self.state.wake.counters().record_present();
                    if let Some(measurement) = self.state.quiescent_efficiency.as_mut() {
                        measurement.record_first_present(Instant::now());
                    }
                } else {
                    // FrameReady signal fired but no texture is pending —
                    // this can happen if acquire_frame() failed on the
                    // compositor thread (error already logged there).
                    tracing::debug!(
                        "main thread: FrameReady received but no pending texture; \
                         compositor likely skipped frame due to surface error"
                    );
                }
            }
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

// ─── Monitor resolution detection ────────────────────────────────────────────

/// Detect the physical size of the primary monitor via the winit event loop.
///
/// Used exclusively for overlay auto-sizing: when `overlay_auto_size` is true
/// and the window mode is `Overlay`, this function is called in `resumed()`
/// before window creation so the overlay covers the full display area.
///
/// ## Resolution order
///
/// 1. `event_loop.primary_monitor()` — the OS-designated primary display.
/// 2. `event_loop.available_monitors().next()` — first enumerated monitor, if
///    no primary is designated (common on some Wayland compositors).
/// 3. `(fallback_width, fallback_height)` — the configured dimensions.
///
/// ## DPI scaling
///
/// `MonitorHandle::size()` returns the **physical** pixel size when the process
/// has per-monitor DPI awareness set (which is guaranteed by the embedded
/// application manifest in `tze_hud_app`). The return value is used directly
/// without scaling — do NOT multiply by `scale_factor()`.
///
/// Background: `MonitorHandle::scale_factor()` calls `GetDpiForMonitor` with
/// `MDT_EFFECTIVE_DPI`, which returns the DPI-awareness-adjusted value (96 for
/// DPI-unaware processes). If the process is somehow not DPI-aware, both
/// `size()` and `scale_factor()` are virtualised; multiplying them produces a
/// doubly-wrong result. The manifest-based DPI awareness declaration (see
/// `app/tze_hud_app/tze_hud.manifest`) ensures physical values at all times.
///
/// ## Errors
///
/// Failures (no monitors detected, headless environment) are logged as warnings
/// and cause a graceful fall back to the configured fallback dimensions.
/// Returns `(width, height, x_position, y_position)` for the selected monitor.
///
/// When `monitor_index` is `Some(i)`, selects the i-th available monitor.
/// When `None`, uses the primary monitor (or first available as fallback).
pub(super) fn detect_monitor_size(
    event_loop: &ActiveEventLoop,
    fallback_width: u32,
    fallback_height: u32,
    monitor_index: Option<usize>,
) -> (u32, u32, i32, i32) {
    // Log available monitors for diagnostics.
    let monitors: Vec<_> = event_loop.available_monitors().collect();
    for (i, m) in monitors.iter().enumerate() {
        let size = m.size();
        let pos = m.position();
        tracing::info!(
            index = i,
            name = m.name().as_deref().unwrap_or("<unnamed>"),
            width = size.width,
            height = size.height,
            x = pos.x,
            y = pos.y,
            scale = m.scale_factor(),
            "available monitor"
        );
    }

    // Select monitor: by index, or primary, or first available.
    let monitor = if let Some(idx) = monitor_index {
        monitors.get(idx).cloned().or_else(|| {
            tracing::warn!(
                requested_index = idx,
                available = monitors.len(),
                "overlay: monitor index out of range, falling back to primary"
            );
            event_loop
                .primary_monitor()
                .or_else(|| monitors.into_iter().next())
        })
    } else {
        event_loop
            .primary_monitor()
            .or_else(|| monitors.into_iter().next())
    };

    match monitor {
        Some(m) => {
            let size = m.size();
            let pos = m.position();
            let scale = m.scale_factor();
            if size.width > 0 && size.height > 0 {
                tracing::info!(
                    monitor_name = m.name().as_deref().unwrap_or("<unnamed>"),
                    physical_width = size.width,
                    physical_height = size.height,
                    position_x = pos.x,
                    position_y = pos.y,
                    scale_factor = scale,
                    "overlay auto-size: selected monitor"
                );
                (size.width, size.height, pos.x, pos.y)
            } else {
                tracing::warn!(
                    fallback_width,
                    fallback_height,
                    "overlay auto-size: monitor size returned (0,0); \
                     using configured fallback dimensions"
                );
                (fallback_width, fallback_height, 0, 0)
            }
        }
        None => {
            tracing::warn!(
                fallback_width,
                fallback_height,
                "overlay auto-size: no monitors detected (headless?); \
                 using configured fallback dimensions"
            );
            (fallback_width, fallback_height, 0, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use tokio::sync::Mutex;
    use tze_hud_scene::NodeData;
    use tze_hud_scene::graph::SceneGraph;

    use super::*;

    #[test]
    fn foreground_lock_timeout_is_restored_after_focus() {
        let saved_timeout = 2_147_483_647;
        let calls = RefCell::new(Vec::new());
        let (focused, attempt) = with_temporary_foreground_lock_timeout(
            || {
                calls.borrow_mut().push("read".to_string());
                Some(saved_timeout)
            },
            |timeout| {
                calls.borrow_mut().push(format!("set:{timeout}"));
                true
            },
            || {
                calls.borrow_mut().push("focus".to_string());
                true
            },
        );

        assert!(focused, "the existing focus sequence must still run");
        assert_eq!(
            calls.into_inner(),
            vec![
                "read".to_string(),
                "set:0".to_string(),
                "focus".to_string(),
                format!("set:{saved_timeout}"),
            ],
            "the saved timeout must be restored after foreground acquisition"
        );
        assert_eq!(
            attempt,
            ForegroundLockTimeoutAttempt {
                previous_timeout: Some(saved_timeout),
                lock_cleared: true,
                lock_restored: Some(true),
            }
        );
        assert!(
            !attempt.setting_may_be_left_altered(),
            "a complete SPI bracket must remain silent in the failure-only diagnostic path"
        );
    }

    #[test]
    fn foreground_lock_timeout_zero_is_not_rewritten() {
        let calls = RefCell::new(Vec::new());
        let (focused, attempt) = with_temporary_foreground_lock_timeout(
            || {
                calls.borrow_mut().push("read".to_string());
                Some(0)
            },
            |timeout| {
                calls.borrow_mut().push(format!("set:{timeout}"));
                true
            },
            || {
                calls.borrow_mut().push("focus".to_string());
                true
            },
        );

        assert!(focused, "the existing focus sequence must still run");
        assert_eq!(
            calls.into_inner(),
            vec!["read".to_string(), "focus".to_string()],
            "an already-clear lock must not incur redundant global SPI writes"
        );
        assert_eq!(
            attempt,
            ForegroundLockTimeoutAttempt {
                previous_timeout: Some(0),
                lock_cleared: true,
                lock_restored: Some(true),
            }
        );
        assert!(
            !attempt.setting_may_be_left_altered(),
            "an already-clear lock must not enter the failure-only diagnostic path"
        );
    }

    #[test]
    fn foreground_lock_timeout_is_not_changed_without_a_saved_value() {
        let calls = RefCell::new(Vec::new());
        let (focused, attempt) = with_temporary_foreground_lock_timeout(
            || {
                calls.borrow_mut().push("read".to_string());
                None
            },
            |timeout| {
                calls.borrow_mut().push(format!("set:{timeout}"));
                true
            },
            || {
                calls.borrow_mut().push("focus".to_string());
                true
            },
        );

        assert!(
            focused,
            "a failed SPI read must preserve the existing focus path"
        );
        assert_eq!(
            calls.into_inner(),
            vec!["read".to_string(), "focus".to_string()],
            "without the previous timeout, the helper must not make an un-restorable change"
        );
        assert!(
            !attempt.setting_may_be_left_altered(),
            "a failed read cannot leave the setting altered and must not produce a false foreground-acquisition warning"
        );
        assert_eq!(
            attempt,
            ForegroundLockTimeoutAttempt {
                previous_timeout: None,
                lock_cleared: false,
                lock_restored: None,
            }
        );
    }

    #[test]
    fn foreground_lock_timeout_is_restored_after_a_failed_clear() {
        let saved_timeout = 2_147_483_647;
        let calls = RefCell::new(Vec::new());
        let set_results = RefCell::new(VecDeque::from([false, true]));
        let (focused, attempt) = with_temporary_foreground_lock_timeout(
            || {
                calls.borrow_mut().push("read".to_string());
                Some(saved_timeout)
            },
            |timeout| {
                calls.borrow_mut().push(format!("set:{timeout}"));
                set_results
                    .borrow_mut()
                    .pop_front()
                    .expect("clear and restore calls must be paired")
            },
            || {
                calls.borrow_mut().push("focus".to_string());
                true
            },
        );

        assert!(
            focused,
            "a failed clear must preserve the existing focus path"
        );
        assert_eq!(
            calls.into_inner(),
            vec![
                "read".to_string(),
                "set:0".to_string(),
                "focus".to_string(),
                format!("set:{saved_timeout}"),
            ],
            "a captured timeout must be restored even when the clear call reports failure"
        );
        assert_eq!(
            attempt,
            ForegroundLockTimeoutAttempt {
                previous_timeout: Some(saved_timeout),
                lock_cleared: false,
                lock_restored: Some(true),
            }
        );
        assert!(
            !attempt.setting_may_be_left_altered(),
            "a failed clear followed by a successful restore cannot leave the setting altered"
        );
    }

    #[test]
    fn foreground_lock_timeout_restore_failure_is_reported() {
        let saved_timeout = 2_147_483_647;
        let calls = RefCell::new(Vec::new());
        let set_results = RefCell::new(VecDeque::from([true, false]));
        let (focused, attempt) = with_temporary_foreground_lock_timeout(
            || {
                calls.borrow_mut().push("read".to_string());
                Some(saved_timeout)
            },
            |timeout| {
                calls.borrow_mut().push(format!("set:{timeout}"));
                set_results
                    .borrow_mut()
                    .pop_front()
                    .expect("clear and restore calls must be paired")
            },
            || {
                calls.borrow_mut().push("focus".to_string());
                true
            },
        );

        assert!(focused, "the existing focus sequence must still run");
        assert_eq!(
            calls.into_inner(),
            vec![
                "read".to_string(),
                "set:0".to_string(),
                "focus".to_string(),
                format!("set:{saved_timeout}"),
            ],
            "a successful clear must still be followed by restoration after focus"
        );
        assert_eq!(
            attempt,
            ForegroundLockTimeoutAttempt {
                previous_timeout: Some(saved_timeout),
                lock_cleared: true,
                lock_restored: Some(false),
            }
        );
        assert!(
            attempt.setting_may_be_left_altered(),
            "only a successful clear followed by a failed restore warrants a foreground-repair warning"
        );
    }

    #[test]
    fn seed_windowed_benchmark_scene_creates_deterministic_tile_stack() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        seed_windowed_benchmark_scene(&mut scene, 1920, 1080);

        assert_eq!(scene.display_area.width, 1920.0);
        assert_eq!(scene.display_area.height, 1080.0);
        assert_eq!(scene.tiles.len(), 20);
        assert!(
            scene.active_tab.is_some(),
            "benchmark scene must select the benchmark tab as active"
        );
        assert!(
            scene
                .leases
                .values()
                .any(|lease| lease.namespace == WINDOWED_BENCHMARK_AGENT),
            "benchmark scene must be owned by the benchmark agent namespace"
        );
        assert!(
            scene.nodes.values().any(|node| {
                matches!(
                    &node.data,
                    NodeData::HitRegion(hit_region)
                        if hit_region
                            .interaction_id
                            .starts_with("windowed-benchmark-input-")
                            && hit_region.accepts_pointer
                )
            }),
            "benchmark scene must include pointer hit regions for input-latency probes"
        );
    }

    #[test]
    fn seed_windowed_benchmark_scene_uses_unique_tab_order() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let existing = scene.create_tab("configured", 0).unwrap();
        assert_eq!(scene.active_tab, Some(existing));

        seed_windowed_benchmark_scene(&mut scene, 1920, 1080);

        assert_eq!(scene.tabs.len(), 2);
        assert_ne!(
            scene.active_tab,
            Some(existing),
            "benchmark mode must activate its own deterministic tab"
        );
        assert_eq!(scene.tiles.len(), 20);
    }

    #[test]
    fn pending_input_latency_drains_into_split_latency_fields() {
        let pending = Arc::new(StdMutex::new(VecDeque::new()));
        let started = Instant::now() - std::time::Duration::from_millis(3);
        let scene_commit_at = Instant::now() - std::time::Duration::from_millis(1);
        record_pending_input_latency(&pending, started, 125);

        let (local_ack, scene_commit, next_present) =
            drain_pending_input_latency(&pending, scene_commit_at, Instant::now())
                .expect("sample drains");

        assert_eq!(local_ack, 125);
        assert!(scene_commit >= 2_000);
        assert!(next_present >= 3_000);
        assert!(scene_commit < next_present);
        assert!(
            drain_pending_input_latency(&pending, scene_commit_at, Instant::now()).is_none(),
            "sample should be consumed exactly once"
        );
    }

    #[test]
    fn pending_input_latency_clamps_sub_microsecond_local_ack_to_nonzero_sample() {
        let pending = Arc::new(StdMutex::new(VecDeque::new()));
        let started = Instant::now() - std::time::Duration::from_millis(1);
        record_pending_input_latency(&pending, started, 0);

        let (local_ack, _, _) =
            drain_pending_input_latency(&pending, Instant::now(), Instant::now())
                .expect("sample drains");

        assert_eq!(local_ack, 1);
    }

    fn make_benchmark_state(warmup: u64, frames: u64) -> WindowedBenchmarkRunState {
        let config = WindowedBenchmarkConfig {
            warmup_frames: warmup,
            frames,
            emit_path: std::env::temp_dir().join(format!(
                "windowed-benchmark-test-{}.json",
                warmup * 1000 + frames
            )),
        };
        WindowedBenchmarkRunState::new(
            config,
            WindowMode::Fullscreen,
            WindowMode::Fullscreen,
            1920,
            1080,
            60,
        )
    }

    #[test]
    fn quiescent_measurement_uses_only_the_post_settle_runtime_counter_delta() {
        let now = Instant::now();
        let config = WindowedQuiescentEfficiencyConfig {
            emit_path: std::env::temp_dir().join("quiescent-efficiency-test.json"),
            build: "test-build".to_string(),
        };
        let adapter = tze_hud_compositor::CompositorAdapterInfo {
            name: "Microsoft Basic Render Driver (WARP)".to_string(),
            backend: "Dx12".to_string(),
            device_type: "Cpu".to_string(),
            driver: "test-driver".to_string(),
            driver_info: "test-driver-info".to_string(),
            vendor: 0,
            device: 0,
        };
        let mut measurement = WindowedQuiescentEfficiencyRunState::new(
            config,
            WindowMode::Overlay,
            640,
            360,
            adapter,
            now,
        );
        let counters = crate::idle_efficiency::IdleEfficiencyCounters::default();

        measurement.record_first_present(now);
        assert_eq!(
            measurement.next_deadline(),
            Some(now + Duration::from_secs(5)),
            "the mandatory five-second settle must begin after an actual present"
        );
        assert!(
            measurement
                .advance(now + Duration::from_millis(4_999), &counters)
                .is_none()
        );

        let settled_at = now + Duration::from_secs(5);
        assert!(measurement.advance(settled_at, &counters).is_none());
        counters.record_main_wakeup(crate::idle_efficiency::RuntimeWakeupSource::SceneChange);
        counters.record_gpu_submission();

        let completed = measurement
            .advance(settled_at + Duration::from_secs(60), &counters)
            .expect("the mandatory sixty-second interval must emit a result");
        assert_eq!(
            completed.artifact.runtime.window_mode,
            EfficiencyWindowMode::Overlay
        );
        assert_eq!(completed.artifact.settling_duration_ms, 5_000);
        assert_eq!(completed.artifact.interval_duration_ms, 60_000);
        assert_eq!(completed.artifact.wakeups.main_loop, 1);
        assert_eq!(completed.artifact.gpu.queue_submissions, 1);
        assert_eq!(completed.artifact.wakeups.excluded_sampler, 1);
        assert!(
            !completed.validation.passed,
            "a real post-settle GPU submission must make the runtime artifact fail"
        );
    }

    #[test]
    fn unchanged_windowed_benchmark_scene_presents_every_requested_sample() {
        let warmup_frames = 2;
        let measured_frames = 3;
        let requested_samples = warmup_frames + measured_frames;
        let mut state = make_benchmark_state(warmup_frames, measured_frames);
        let mut completed = false;

        for frame_number in 1..=requested_samples {
            assert!(
                windowed_frame_needs_render(false, false, false, false, true, false),
                "explicit benchmark mode must bypass the unchanged-scene idle gate for sample {frame_number}"
            );
            let mut telemetry = tze_hud_telemetry::FrameTelemetry::new(frame_number);
            telemetry.frame_time_us = 8_000;
            telemetry.stage7_gpu_submit_us = 1;
            completed = state.record(&telemetry);
            assert_eq!(
                completed,
                frame_number == requested_samples,
                "benchmark completion must occur on exactly the final requested sample"
            );
        }

        assert!(
            completed,
            "the final requested benchmark sample must complete the run"
        );
        assert_eq!(state.warmup_seen, warmup_frames);
        assert_eq!(state.measured_seen, measured_frames);
        assert_eq!(state.summary.total_frames, measured_frames);
        assert!(
            !windowed_frame_needs_render(false, false, false, false, false, false),
            "normal unchanged runtime sessions must remain behind the idle render gate"
        );
    }

    #[test]
    fn successful_surface_recovery_repaints_an_unchanged_scene() {
        assert!(
            windowed_frame_needs_render(false, false, false, false, false, true),
            "a reconfigured surface needs one fresh frame even when scene state is unchanged"
        );
    }

    #[test]
    fn surface_repaint_requirement_survives_lock_miss_until_submit() {
        let pending_after_recovery = update_surface_repaint_pending(false, true, false);
        assert!(pending_after_recovery);
        assert!(
            windowed_frame_needs_render(false, false, false, false, false, pending_after_recovery),
            "the recovered surface must request a fresh frame even for an unchanged scene"
        );

        let pending_after_lock_miss =
            update_surface_repaint_pending(pending_after_recovery, false, false);
        assert!(
            pending_after_lock_miss,
            "a scene-lock miss must not discard the recovered-surface repaint requirement"
        );

        assert!(
            !update_surface_repaint_pending(pending_after_lock_miss, false, true),
            "the repaint requirement clears only after a frame submits"
        );
    }

    #[test]
    fn windowed_benchmark_does_not_count_skipped_present_attempt() {
        let mut state = make_benchmark_state(0, 2);
        let mut skipped_attempt = tze_hud_telemetry::FrameTelemetry::new(1);
        skipped_attempt.frame_time_us = 8_000;
        assert!(!state.record(&skipped_attempt));
        assert_eq!(
            state.measured_seen, 0,
            "telemetry without a completed Stage 7 submit/present must not advance benchmark progress"
        );
    }

    #[test]
    fn benchmark_watchdog_fires_when_no_frame_recorded_past_timeout() {
        let state = make_benchmark_state(0, 10);
        // Sleep long enough that elapsed() > the test timeout.
        std::thread::sleep(Duration::from_millis(5));
        assert!(
            state.is_stalled(Duration::from_millis(1)),
            "watchdog must fire when no frame recorded past the timeout"
        );
    }

    #[test]
    fn benchmark_watchdog_does_not_fire_immediately_after_record() {
        let mut state = make_benchmark_state(0, 10);
        // Age the creation timestamp well past a short threshold.
        std::thread::sleep(Duration::from_millis(5));
        let mut telem = tze_hud_telemetry::FrameTelemetry::new(1);
        telem.frame_time_us = 8_000;
        telem.stage7_gpu_submit_us = 1;
        state.record(&telem);
        // Immediately after record(), last_frame_at is fresh — generous threshold.
        assert!(
            !state.is_stalled(Duration::from_millis(100)),
            "watchdog must not fire immediately after a frame is recorded"
        );
    }

    #[test]
    fn benchmark_watchdog_resets_on_warmup_frame() {
        let mut state = make_benchmark_state(1, 10);
        // Age the creation timestamp.
        std::thread::sleep(Duration::from_millis(5));
        let mut telem = tze_hud_telemetry::FrameTelemetry::new(1);
        telem.frame_time_us = 8_000;
        telem.stage7_gpu_submit_us = 1;
        let done = state.record(&telem); // this is a warmup frame
        assert!(!done, "warmup frame must not complete the benchmark");
        assert!(
            !state.is_stalled(Duration::from_millis(100)),
            "watchdog must reset on warmup frames too"
        );
    }

    #[test]
    fn benchmark_watchdog_emit_abort_writes_diagnostic_json() {
        let emit_path = std::env::temp_dir().join("windowed-benchmark-watchdog-abort-test.json");
        let config = WindowedBenchmarkConfig {
            warmup_frames: 2,
            frames: 10,
            emit_path: emit_path.clone(),
        };
        let state = WindowedBenchmarkRunState::new(
            config,
            WindowMode::Fullscreen,
            WindowMode::Fullscreen,
            1920,
            1080,
            60,
        );
        state
            .emit_watchdog_abort("no-progress timeout")
            .expect("emit_watchdog_abort must succeed");
        let contents = std::fs::read_to_string(&emit_path).expect("file must be written");
        let json: serde_json::Value =
            serde_json::from_str(&contents).expect("output must be valid JSON");
        assert_eq!(
            json["schema"], "tze_hud.windowed_compositor_benchmark.v1",
            "schema field must be present"
        );
        let abort = &json["watchdog_abort"];
        assert_eq!(
            abort["reason"], "no-progress timeout",
            "abort reason must be preserved"
        );
        assert_eq!(
            abort["warmup_frames_seen"], 0,
            "no warmup frames were recorded"
        );
        assert_eq!(
            abort["measured_frames_seen"], 0,
            "no measured frames were recorded"
        );
        assert!(
            abort["timeout_secs"].as_u64().unwrap_or(0) > 0,
            "timeout_secs must be positive"
        );
        assert!(
            json.get("frame_time").is_none(),
            "watchdog abort must not include frame_time (no frames recorded)"
        );
    }

    #[test]
    fn windowed_benchmark_records_nonzero_split_input_latency() {
        let config = WindowedBenchmarkConfig {
            warmup_frames: 0,
            frames: 10,
            emit_path: std::env::temp_dir().join("windowed-benchmark-test.json"),
        };
        let mut state = WindowedBenchmarkRunState::new(
            config,
            WindowMode::Overlay,
            WindowMode::Overlay,
            1920,
            1080,
            60,
        );
        let mut telemetry = tze_hud_telemetry::FrameTelemetry::new(1);
        telemetry.frame_time_us = 12_000;
        telemetry.stage7_gpu_submit_us = 1;
        telemetry.tile_count = 3;
        telemetry.input_to_local_ack_us = 900;
        telemetry.input_to_scene_commit_us = 10_500;
        telemetry.input_to_next_present_us = 18_000;

        assert!(!state.record(&telemetry));
        assert_eq!(
            state
                .summary
                .input_to_local_ack
                .samples
                .iter()
                .copied()
                .collect::<Vec<u64>>(),
            vec![900]
        );
        assert_eq!(
            state
                .summary
                .input_to_scene_commit
                .samples
                .iter()
                .copied()
                .collect::<Vec<u64>>(),
            vec![10_500]
        );
        assert_eq!(
            state
                .summary
                .input_to_next_present
                .samples
                .iter()
                .copied()
                .collect::<Vec<u64>>(),
            vec![18_000]
        );
    }

    #[test]
    fn windowed_benchmark_does_not_treat_zero_input_latency_as_sample() {
        let config = WindowedBenchmarkConfig {
            warmup_frames: 0,
            frames: 10,
            emit_path: std::env::temp_dir().join("windowed-benchmark-test.json"),
        };
        let mut state = WindowedBenchmarkRunState::new(
            config,
            WindowMode::Overlay,
            WindowMode::Overlay,
            1920,
            1080,
            60,
        );
        let mut telemetry = tze_hud_telemetry::FrameTelemetry::new(1);
        telemetry.frame_time_us = 12_000;
        telemetry.stage7_gpu_submit_us = 1;

        assert!(!state.record(&telemetry));
        assert!(state.summary.input_to_local_ack.samples.is_empty());
        assert!(state.summary.input_to_scene_commit.samples.is_empty());
        assert!(state.summary.input_to_next_present.samples.is_empty());
    }

    /// `spin_acquire` is the bounded-wait acquisition used on the main-thread
    /// interactive-feedback path (active drag move / resize step).  It must:
    /// acquire immediately when free, wait out a brief contending holder and
    /// still acquire within budget, and give up (return `None`) — never
    /// hard-block — once a holder outlasts the budget.  This is what keeps a
    /// dragged window tracking the pointer instead of jumping in bursts, and a
    /// `Ctrl+`/`-` resize landing the same frame, without ever freezing the UI
    /// thread on a pathological lock holder (hud-0xudd).  Deliberately a plain
    /// `#[test]` (no runtime) to also assert it works off the async runtime.
    #[test]
    fn spin_acquire_waits_out_brief_holder_but_yields_on_overrun() {
        // Free lock → immediate Some.
        let m: Arc<Mutex<u32>> = Arc::new(Mutex::new(7));
        {
            let g =
                spin_acquire(&m, Duration::from_millis(50)).expect("free mutex acquires at once");
            assert_eq!(*g, 7);
        }

        // Holder releases after a brief hold → spin_acquire waits it out.
        let m2 = Arc::clone(&m);
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let g = m2.try_lock().expect("holder takes the free lock");
            acquired_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(10));
            drop(g);
        });
        acquired_rx.recv().unwrap(); // ensure the holder owns the lock first
        assert!(
            spin_acquire(&m, Duration::from_secs(2)).is_some(),
            "spin_acquire must wait out a brief holder and acquire within budget"
        );
        holder.join().unwrap();

        // Holder outlasts the budget → spin_acquire gives up promptly.
        let m3 = Arc::clone(&m);
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let g = m3.try_lock().expect("holder takes the free lock");
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap(); // hold until the assertion completes
            drop(g);
        });
        held_rx.recv().unwrap();
        let start = Instant::now();
        let guard = spin_acquire(&m, Duration::from_millis(10));
        let elapsed = start.elapsed();
        assert!(
            guard.is_none(),
            "spin_acquire must return None once the budget elapses"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "spin_acquire must return promptly after the budget, not hard-block"
        );
        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }
}
