//! Hide from screen capture (spec R5.2): one owner switch that keeps every
//! overlay window, and so the control panel and the hidden pill drawn in
//! them, out of screen captures and shares while the owner still sees it.
//!
//! The policy is platform-neutral and tested here: which affinity every
//! overlay should have, when nothing may be called, and what `/admin/status`
//! reports. The main thread owns the windows and applies the affinity through
//! an injected setter (`SetWindowDisplayAffinity` on Windows), so nothing
//! polls: the panel flips the switch, the session watcher reports a remote
//! session, and either wakes the main thread once.
//!
//! An owner view setting, not a privacy layer: agents are not told and no
//! content is filtered. `/admin/screenshot` reads the compositor back and is
//! unaffected.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde_json::{Value, json};
use tze_hud_scene::render_wake::RenderWakeNotifier;

/// Windows 10 version 2004. Earlier builds treat `WDA_EXCLUDEFROMCAPTURE` as
/// `WDA_MONITOR`: a black box in captures instead of nothing.
pub(crate) const FLOOR_BUILD: u32 = 19041;

/// The disabled row's reason below [`FLOOR_BUILD`].
pub(crate) const BELOW_FLOOR: &str = "Needs Windows 10 version 2004 or later";

/// The display affinity an overlay window should have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Affinity {
    /// `WDA_NONE`: captures show the window.
    None,
    /// `WDA_EXCLUDEFROMCAPTURE`: captures show what is behind it.
    ExcludeFromCapture,
}

/// The last application's result, for `/admin/status` and the row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Outcome {
    /// Every overlay took `WDA_EXCLUDEFROMCAPTURE`.
    applied: bool,
    /// Display indices (0 = primary) whose window refused the affinity.
    failed: Vec<usize>,
}

pub(crate) struct CaptureExclusion {
    /// The Windows build; `None` off Windows (nothing to exclude, no row).
    build: Option<u32>,
    on: AtomicBool,
    /// The Windows session is remote: an RDP viewer must keep seeing the
    /// HUD, so every overlay stays `WDA_NONE` even with the switch on.
    remote: AtomicBool,
    generation: AtomicU64,
    outcome: Mutex<Outcome>,
    /// Wakes the main thread, which owns the windows.
    wake: RenderWakeNotifier,
}

impl Default for CaptureExclusion {
    fn default() -> Self {
        Self::new(None, false, RenderWakeNotifier::default())
    }
}

impl CaptureExclusion {
    pub(crate) fn new(build: Option<u32>, on: bool, wake: RenderWakeNotifier) -> Self {
        Self {
            build,
            on: on.into(),
            remote: false.into(),
            generation: 0.into(),
            outcome: Mutex::default(),
            wake,
        }
    }

    /// This machine's build (`RtlGetVersion` on Windows, none elsewhere),
    /// already remote when the HUD starts inside a Remote Desktop session,
    /// so its first frames are never hidden from the remote viewer.
    pub(crate) fn for_platform(on: bool, wake: RenderWakeNotifier) -> Self {
        let exclusion = Self::new(platform_build(), on, wake);
        exclusion.remote.store(platform_remote(), Ordering::Release);
        exclusion
    }

    /// There are overlay windows to exclude (Windows).
    pub(crate) fn available(&self) -> bool {
        self.build.is_some()
    }

    /// The build is at or above [`FLOOR_BUILD`].
    pub(crate) fn supported(&self) -> bool {
        self.build.is_some_and(|build| build >= FLOOR_BUILD)
    }

    pub(crate) fn is_on(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    /// The owner's switch; applies from the main thread's next turn.
    pub(crate) fn set_on(&self, on: bool) {
        if self.on.swap(on, Ordering::AcqRel) != on {
            self.changed();
        }
    }

    /// The session became remote (`WTS_REMOTE_CONNECT`, or
    /// `SM_REMOTESESSION` at start) or returned to the console.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn set_remote(&self, remote: bool) {
        if self.remote.swap(remote, Ordering::AcqRel) != remote {
            self.changed();
        }
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn is_remote(&self) -> bool {
        self.remote.load(Ordering::Acquire)
    }

    /// Overlay windows closed: display indices shifted, so the next main
    /// turn reapplies to the remaining windows and rebuilds `failed`.
    pub(crate) fn windows_removed(&self) {
        self.changed();
    }

    fn changed(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.wake.notify();
    }

    /// Why the switch, when on, does not exclude: `"unsupported"` below the
    /// floor, `"remote"` in a remote session.
    pub(crate) fn reason(&self) -> Option<&'static str> {
        if !self.supported() {
            Some("unsupported")
        } else if self.remote.load(Ordering::Acquire) {
            Some("remote")
        } else {
            None
        }
    }

    /// The affinity every overlay should have now.
    pub(crate) fn wanted(&self) -> Affinity {
        if self.is_on() && self.reason().is_none() {
            Affinity::ExcludeFromCapture
        } else {
            Affinity::None
        }
    }

    /// Whether the switch or the session changed since `seen` (updated).
    pub(crate) fn take_change(&self, seen: &mut u64) -> bool {
        let now = self.generation.load(Ordering::Acquire);
        std::mem::replace(seen, now) != now
    }

    /// Give every overlay `(display index, window)` the wanted affinity.
    /// Below the floor nothing is called at all.
    pub(crate) fn apply_all<W>(
        &self,
        windows: impl IntoIterator<Item = (usize, W)>,
        mut set: impl FnMut(&W, Affinity) -> Result<(), String>,
    ) {
        if !self.supported() {
            *self.outcome() = Outcome::default();
            return;
        }
        let want = self.wanted();
        let mut failed = Vec::new();
        for (index, window) in windows {
            if let Err(error) = set(&window, want) {
                tracing::warn!(index, ?want, %error, "capture exclusion: affinity refused");
                failed.push(index);
            }
        }
        tracing::info!(?want, ?failed, "capture exclusion: overlays updated");
        *self.outcome() = Outcome {
            applied: want == Affinity::ExcludeFromCapture && failed.is_empty(),
            failed,
        };
    }

    /// A window just opened (start, hot-plug, recreate after surface loss),
    /// before its first present: excluded at once while exclusion is in
    /// force. A new window already has `WDA_NONE`, so otherwise nothing runs.
    pub(crate) fn apply_new<W>(
        &self,
        index: usize,
        window: &W,
        set: impl FnOnce(&W, Affinity) -> Result<(), String>,
    ) {
        if self.wanted() != Affinity::ExcludeFromCapture {
            return;
        }
        let result = set(window, Affinity::ExcludeFromCapture);
        let mut outcome = self.outcome();
        outcome.failed.retain(|i| *i != index);
        if let Err(error) = result {
            tracing::warn!(index, %error, "capture exclusion: new overlay refused affinity");
            outcome.failed.push(index);
        }
        outcome.applied = outcome.failed.is_empty();
    }

    fn outcome(&self) -> std::sync::MutexGuard<'_, Outcome> {
        self.outcome.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Display indices whose window refused the affinity last time.
    pub(crate) fn failed(&self) -> Vec<usize> {
        self.outcome().failed.clone()
    }

    /// `/admin/status` `capture_exclusion`: null off Windows.
    pub(crate) fn status_json(&self) -> Value {
        if !self.available() {
            return Value::Null;
        }
        let on = self.is_on();
        let outcome = self.outcome().clone();
        json!({
            "on": on,
            "supported": self.supported(),
            "applied": on && outcome.applied && self.wanted() == Affinity::ExcludeFromCapture,
            "reason": if on { self.reason() } else { None },
            "failed": outcome.failed,
        })
    }
}

/// Whether the session is remote after `WM_WTSSESSION_CHANGE` `code`:
/// `WTS_REMOTE_CONNECT` (3) makes it remote, `WTS_CONSOLE_CONNECT` (1) local;
/// disconnects, locks and the rest leave it as it was.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn remote_after(code: u32, remote: bool) -> bool {
    match code {
        3 => true,
        1 => false,
        _ => remote,
    }
}

/// The installed instance, for `/admin/status` (one HUD per process).
static GLOBAL: std::sync::OnceLock<std::sync::Arc<CaptureExclusion>> = std::sync::OnceLock::new();

pub(crate) fn install(exclusion: std::sync::Arc<CaptureExclusion>) {
    if GLOBAL.set(exclusion).is_err() {
        tracing::warn!("capture exclusion installed twice; keeping the first");
    }
}

/// `capture_exclusion` for `/admin/status`.
pub(crate) fn status() -> Value {
    GLOBAL.get().map_or(Value::Null, |c| c.status_json())
}

/// The installed instance, for the session watcher.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn global() -> Option<std::sync::Arc<CaptureExclusion>> {
    GLOBAL.get().cloned()
}

#[cfg(target_os = "windows")]
fn platform_build() -> Option<u32> {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a correctly sized, writable OSVERSIONINFOW.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status.is_ok() {
        Some(info.dwBuildNumber)
    } else {
        // Unknown: treat as below the floor, so nothing risks a black box.
        tracing::warn!(?status, "capture exclusion: Windows build unknown");
        Some(0)
    }
}

#[cfg(not(target_os = "windows"))]
fn platform_build() -> Option<u32> {
    None
}

/// The process runs in a Remote Desktop session (`SM_REMOTESESSION`).
#[cfg(target_os = "windows")]
pub(crate) fn platform_remote() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
    // SAFETY: a plain system-metrics query.
    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn platform_remote() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    fn exclusion(build: u32, on: bool) -> (CaptureExclusion, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&wakes);
        let wake = RenderWakeNotifier::new(move || {
            w.fetch_add(1, Ordering::SeqCst);
        });
        (CaptureExclusion::new(Some(build), on, wake), wakes)
    }

    type Log = RefCell<Vec<(usize, Affinity)>>;

    fn setter(
        log: &Log,
        refuse: Option<usize>,
    ) -> impl FnMut(&usize, Affinity) -> Result<(), String> + '_ {
        move |window, affinity| {
            log.borrow_mut().push((*window, affinity));
            if Some(*window) == refuse {
                Err("Access is denied.".into())
            } else {
                Ok(())
            }
        }
    }

    /// R5.2 S1/S3: on excludes every overlay (primary and secondaries) and
    /// status reports it applied; off returns every overlay to WDA_NONE.
    /// One switch flip wakes the main thread once.
    #[test]
    fn switch_excludes_every_overlay_and_off_restores_them() {
        let (c, wakes) = exclusion(22631, false);
        let windows = || [(0, 0usize), (1, 1), (2, 2)];
        let mut seen = 0;
        assert!(!c.take_change(&mut seen), "nothing to apply at start");

        c.set_on(true);
        c.set_on(true);
        assert_eq!(wakes.load(Ordering::SeqCst), 1, "one wake per change");
        assert!(c.take_change(&mut seen));
        assert!(!c.take_change(&mut seen));
        let log = RefCell::new(Vec::new());
        c.apply_all(windows(), setter(&log, None));
        assert_eq!(
            *log.borrow(),
            (0..3)
                .map(|i| (i, Affinity::ExcludeFromCapture))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            c.status_json(),
            json!({"on": true, "supported": true, "applied": true, "reason": null, "failed": []})
        );

        c.set_on(false);
        assert!(c.take_change(&mut seen));
        log.borrow_mut().clear();
        c.apply_all(windows(), setter(&log, None));
        assert!(log.borrow().iter().all(|(_, a)| *a == Affinity::None));
        assert_eq!(log.borrow().len(), 3);
        assert_eq!(c.status_json()["applied"], json!(false));
        assert_eq!(c.status_json()["on"], json!(false));
    }

    /// R5.2 S2: a monitor plugged in or an overlay recreated while on is
    /// excluded before its first present; while off nothing is called. A
    /// refused window is reported by display index and cleared when it
    /// later succeeds.
    #[test]
    fn new_windows_are_excluded_at_once_and_failures_are_reported() {
        let (c, _) = exclusion(19041, false);
        let log = RefCell::new(Vec::new());
        c.apply_new(1, &1usize, setter(&log, None));
        assert!(log.borrow().is_empty(), "off: a new window keeps WDA_NONE");

        c.set_on(true);
        c.apply_all([(0, 0usize), (1, 1)], setter(&log, Some(1)));
        assert_eq!(c.failed(), [1]);
        assert_eq!(c.status_json()["applied"], json!(false));
        assert_eq!(c.status_json()["failed"], json!([1]));

        log.borrow_mut().clear();
        c.apply_new(1, &1usize, setter(&log, None));
        c.apply_new(2, &2usize, setter(&log, None));
        assert_eq!(
            *log.borrow(),
            [
                (1, Affinity::ExcludeFromCapture),
                (2, Affinity::ExcludeFromCapture)
            ]
        );
        assert!(c.failed().is_empty());
        assert_eq!(c.status_json()["applied"], json!(true));

        // Display 1 refuses, then display 1 is unplugged: display 2 becomes
        // 1. The removal schedules a reapply, which rebuilds `failed` with
        // the shifted indices instead of blaming the new display 1.
        c.apply_all([(0, 0usize), (1, 1), (2, 2)], setter(&log, Some(1)));
        assert_eq!(c.failed(), [1]);
        let mut seen = c.generation.load(Ordering::Acquire);
        c.windows_removed();
        assert!(c.take_change(&mut seen));
        c.apply_all([(0, 0usize), (1, 2)], setter(&log, None));
        assert!(c.failed().is_empty());
        assert_eq!(c.status_json()["applied"], json!(true));
    }

    /// Off Windows a session is never remote; on Windows it is read once at
    /// start.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn off_windows_the_session_is_local() {
        assert!(!platform_remote());
        assert!(!CaptureExclusion::for_platform(true, RenderWakeNotifier::default()).is_remote());
    }

    /// R5.2 S5: below build 19041 nothing is ever called (an older build
    /// would draw a black box), and status says unsupported.
    #[test]
    fn below_the_floor_no_affinity_call_is_made() {
        let (c, _) = exclusion(18363, true);
        let log = RefCell::new(Vec::new());
        c.apply_all([(0, 0usize), (1, 1)], setter(&log, None));
        c.apply_new(2, &2usize, setter(&log, None));
        assert!(log.borrow().is_empty());
        assert_eq!(
            c.status_json(),
            json!({"on": true, "supported": false, "applied": false, "reason": "unsupported", "failed": []})
        );
        assert_eq!(c.wanted(), Affinity::None);
    }

    /// R5.2 S7: a remote session holds every overlay at WDA_NONE with the
    /// switch on (an RDP viewer keeps seeing the HUD) and status says why;
    /// back on the console the exclusion is applied again.
    #[test]
    fn a_remote_session_holds_wda_none_and_the_console_reapplies() {
        let (c, wakes) = exclusion(22631, true);
        let windows = || [(0, 0usize), (1, 1)];
        let mut seen = 0;
        let log = RefCell::new(Vec::new());
        c.apply_all(windows(), setter(&log, None));

        c.set_remote(true);
        assert!(c.take_change(&mut seen));
        log.borrow_mut().clear();
        c.apply_all(windows(), setter(&log, None));
        assert_eq!(*log.borrow(), [(0, Affinity::None), (1, Affinity::None)]);
        assert_eq!(
            c.status_json(),
            json!({"on": true, "supported": true, "applied": false, "reason": "remote", "failed": []})
        );
        log.borrow_mut().clear();
        c.apply_new(2, &2usize, setter(&log, None));
        assert!(log.borrow().is_empty(), "remote: new windows stay WDA_NONE");

        c.set_remote(false);
        assert!(c.take_change(&mut seen));
        c.apply_all(windows(), setter(&log, None));
        assert_eq!(
            *log.borrow(),
            [
                (0, Affinity::ExcludeFromCapture),
                (1, Affinity::ExcludeFromCapture)
            ]
        );
        assert_eq!(c.status_json()["applied"], json!(true));
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }

    /// Only a remote or console connect changes the remote bit.
    #[test]
    fn session_codes_set_and_clear_remote() {
        assert!(remote_after(3, false));
        assert!(!remote_after(1, true));
        for code in [2, 4, 7, 8, 99] {
            assert!(remote_after(code, true) && !remote_after(code, false));
        }
    }

    /// Off Windows there is nothing to exclude: status is null.
    #[test]
    fn off_windows_status_is_null() {
        assert_eq!(CaptureExclusion::default().status_json(), Value::Null);
        assert!(!CaptureExclusion::default().available());
    }
}
