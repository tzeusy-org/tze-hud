#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
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

use crate::channels::{InputEvent, InputEventKind};
use crate::window::WindowMode;

use super::input_dispatch::{
    dispatch_capture_released_event, dispatch_focus_event, dispatch_pointer_event,
    dispatch_portal_geometry_event, dispatch_scroll_offset_event, enqueue_input,
    nanoseconds_since_start,
};
use super::keyboard::ComposerDeliveryContext;
use super::portal::{
    DragReleasedData, PortalResizePointerOutcome, PrimaryDragGesture,
    apply_drag_handle_pointer_event, apply_portal_resize_pointer_event, drag_feedback_target,
    reconcile_drag_feedback,
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

// Native capture history is retained until its matching application edge is
// delivered. This domain is UI-thread-only; no app/scene pointer crosses FFI.
#[cfg(any(target_os = "windows", test))]
#[derive(Default)]
pub(super) struct ChromeCaptureLedger {
    faulted: bool,
    next_install: u64,
    next_frame: u64,
    app_armed: Option<(u64, std::rc::Rc<ChromeCapturePress>)>,
    last_retired: bool,
    installs: Vec<ChromeCaptureInstall>,
    active: Vec<ChromeCaptureReceipt>,
    completed: std::collections::VecDeque<ChromeCaptureReceipt>,
}

#[cfg(any(target_os = "windows", test))]
struct ChromeCaptureInstall {
    nonce: u64,
    window: winit::window::WindowId,
    enabled: bool,
    generation: u64,
    native: Option<std::rc::Rc<ChromeCapturePress>>,
    app: Option<std::rc::Rc<ChromeCapturePress>>,
    permit: bool,
}

#[cfg(any(target_os = "windows", test))]
struct ChromeCapturePress {
    generation: u64,
    cancelled: std::cell::Cell<bool>,
    finished: std::cell::Cell<bool>,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone)]
struct ChromeCaptureReceipt {
    frame: u64,
    nonce: u64,
    window: winit::window::WindowId,
    down: bool,
    token: Option<std::rc::Rc<ChromeCapturePress>>,
    consumed: bool,
}

#[cfg(any(target_os = "windows", test))]
impl ChromeCaptureLedger {
    const MAX_INSTALLATIONS: usize = 64;
    const MAX_RECEIPTS: usize = 256;

    pub(super) fn fault(&mut self) {
        self.faulted = true;
        for install in &mut self.installs {
            install.permit = false;
            for token in install.native.iter().chain(install.app.iter()) {
                token.cancelled.set(true);
            }
        }
        for receipt in self.active.iter().chain(self.completed.iter()) {
            if let Some(token) = &receipt.token {
                token.cancelled.set(true);
            }
        }
    }

    pub(super) fn install(&mut self, window: winit::window::WindowId) -> Option<u64> {
        self.prune();
        if self.faulted || self.installs.len() == Self::MAX_INSTALLATIONS {
            self.fault();
            return None;
        }
        let Some(nonce) = self.next_install.checked_add(1) else {
            self.fault();
            return None;
        };
        self.next_install = nonce;
        self.installs.push(ChromeCaptureInstall {
            nonce,
            window,
            enabled: true,
            generation: 0,
            native: None,
            app: None,
            permit: false,
        });
        Some(nonce)
    }

    pub(super) fn begin(&mut self, nonce: u64, down: bool) -> Option<u64> {
        if self.faulted {
            return None;
        }
        if self.active.len() + self.completed.len() >= Self::MAX_RECEIPTS {
            self.fault();
            return None;
        }
        let Some(frame) = self.next_frame.checked_add(1) else {
            self.fault();
            return None;
        };
        let i = self.installs.iter().position(|s| s.nonce == nonce && s.enabled)?;
        if down {
            let Some(generation) = self.installs[i].generation.checked_add(1) else {
                self.fault();
                return None;
            };
            let install = &mut self.installs[i];
            if let Some(previous) = &install.native {
                if !previous.finished.get() {
                    previous.cancelled.set(true);
                }
            }
            install.generation = generation;
            install.native = Some(std::rc::Rc::new(ChromeCapturePress {
                generation,
                cancelled: std::cell::Cell::new(false),
                finished: std::cell::Cell::new(false),
            }));
        }
        self.next_frame = frame;
        let install = &self.installs[i];
        self.active.push(ChromeCaptureReceipt {
            frame,
            nonce,
            window: install.window,
            down,
            token: install.native.clone(),
            consumed: false,
        });
        Some(frame)
    }

    pub(super) fn complete(&mut self, frame: u64) {
        let Some(i) = self.active.iter().position(|r| r.frame == frame) else {
            self.fault();
            return;
        };
        let receipt = self.active.remove(i);
        if !receipt.consumed {
            self.completed.push_back(receipt);
        }
        self.prune();
    }

    pub(super) fn consume(&mut self, window: winit::window::WindowId, down: bool) -> bool {
        self.last_retired = false;
        if self.faulted {
            return false;
        }
        let pending = self.completed.iter().position(|r| r.window == window);
        let active = self
            .active
            .iter()
            .rposition(|r| r.window == window && !r.consumed);
        if pending.is_some() && active.is_some() {
            self.fault();
            return false;
        }
        let receipt = if let Some(i) = pending {
            self.completed.remove(i).expect("selected receipt exists")
        } else if let Some(i) = active {
            self.active[i].consumed = true;
            self.active[i].clone()
        } else {
            self.fault();
            return false;
        };
        if receipt.down != down {
            self.fault();
            return false;
        }
        let Some(i) = self.installs.iter().position(|s| s.nonce == receipt.nonce) else {
            self.fault();
            return false;
        };
        if !self.installs[i].enabled {
            self.last_retired = true;
            self.prune();
            return false;
        }
        self.installs[i].permit = false;
        let Some(token) = receipt.token else {
            return false;
        };
        if token.cancelled.get() || token.finished.get() {
            return false;
        }
        if down {
            // Only one physical left press is application-armed. Old tokens
            // retain their history, but cannot later clear the newer press.
            for install in &mut self.installs {
                install.app = None;
            }
            self.installs[i].app = Some(token.clone());
            self.app_armed = Some((receipt.nonce, token));
            self.installs[i].permit = true;
        } else if self.app_armed.as_ref().is_some_and(|(nonce, armed)| {
            *nonce == receipt.nonce
                && std::rc::Rc::ptr_eq(armed, &token)
                && armed.generation == token.generation
        }) {
            token.finished.set(true);
            self.installs[i].app = None;
            self.app_armed = None;
            self.installs[i].permit = true;
        }
        self.installs[i].permit
    }

    pub(super) fn lose(&mut self, nonce: u64) -> bool {
        let Some(install) = self.installs.iter_mut().find(|s| s.nonce == nonce) else {
            return false;
        };
        let Some(token) = &install.native else {
            return false;
        };
        if token.finished.get() || token.cancelled.replace(true) {
            return false;
        }
        install.permit = false;
        true
    }

    pub(super) fn permit(&self, window: winit::window::WindowId) -> bool {
        !self.faulted
            && self
                .installs
                .iter()
                .rev()
                .find(|s| s.window == window && s.enabled)
                .is_some_and(|s| s.permit && s.app.as_ref().is_none_or(|t| !t.cancelled.get()))
    }

    pub(super) fn take_cancel(&mut self, window: winit::window::WindowId) -> bool {
        let Some((nonce, token)) = &self.app_armed else {
            return false;
        };
        if !self
            .installs
            .iter()
            .any(|s| s.nonce == *nonce && s.window == window)
            || (!self.faulted && !token.cancelled.get())
        {
            return false;
        }
        let nonce = *nonce;
        self.app_armed = None;
        if let Some(install) = self.installs.iter_mut().find(|s| s.nonce == nonce) {
            install.app = None;
            install.permit = false;
        }
        self.prune();
        true
    }

    pub(super) fn finish_synthetic(&mut self, window: winit::window::WindowId) {
        let Some((nonce, token)) = &self.app_armed else {
            return;
        };
        if !self
            .installs
            .iter()
            .any(|s| s.nonce == *nonce && s.window == window)
        {
            return;
        }
        token.finished.set(true);
        let nonce = *nonce;
        self.app_armed = None;
        if let Some(install) = self.installs.iter_mut().find(|s| s.nonce == nonce) {
            install.app = None;
        }
    }

    pub(super) fn retire(&mut self, nonce: u64) {
        if let Some(install) = self.installs.iter_mut().find(|s| s.nonce == nonce) {
            install.enabled = false;
            install.permit = false;
            for token in install.native.iter().chain(install.app.iter()) {
                token.cancelled.set(true);
            }
        }
        for receipt in self
            .active
            .iter()
            .chain(self.completed.iter())
            .filter(|r| r.nonce == nonce)
        {
            if let Some(token) = &receipt.token {
                token.cancelled.set(true);
            }
        }
        self.prune();
    }

    fn prune(&mut self) {
        self.installs.retain(|s| {
            s.enabled
                || s.app.is_some()
                || self.active.iter().any(|r| r.nonce == s.nonce)
                || self.completed.iter().any(|r| r.nonce == s.nonce)
        });
    }
}

#[cfg(target_os = "windows")]
std::thread_local! {
    static CHROME_NATIVE_OWNER: std::cell::RefCell<ChromeNativeOwner> =
        std::cell::RefCell::new(ChromeNativeOwner::default());
    static CHROME_NATIVE_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CHROME_NATIVE_FAULT_NOTIFIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(target_os = "windows")]
#[derive(Default)]
struct ChromeNativeOwner {
    ledger: ChromeCaptureLedger,
    app_edge: Option<bool>,
    fault_delivered: bool,
    registrations: Vec<std::rc::Weak<ChromeNativeRegistration>>,
}

// Exactly one raw Rc reference belongs to a successful native registration.
// This object deliberately stores no Arc<Window> or pointer to runtime state.
#[cfg(target_os = "windows")]
struct ChromeNativeRegistration {
    hwnd: windows::Win32::Foundation::HWND,
    nonce: u64,
    installed: std::cell::Cell<bool>,
    remove_attempted: std::cell::Cell<bool>,
    expected_normal_release: std::cell::Cell<bool>,
    wake: tze_hud_scene::render_wake::RenderWakeNotifier,
}

#[cfg(target_os = "windows")]
const CHROME_CAPTURE_SUBCLASS_ID: usize = 0x545a4348;

#[cfg(target_os = "windows")]
fn chrome_native_fault() {
    let first = CHROME_NATIVE_FAULT.with(|fault| !fault.replace(true));
    let wakes = CHROME_NATIVE_OWNER.with(|owner| {
        let Ok(mut owner) = owner.try_borrow_mut() else {
            return Vec::new();
        };
        owner.ledger.fault();
        owner
            .registrations
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .map(|registration| registration.wake.clone())
            .collect::<Vec<_>>()
    });
    if first {
        tracing::error!(
            "notification capture history unavailable; notification actions disabled until restart"
        );
    }
    if !wakes.is_empty() && CHROME_NATIVE_FAULT_NOTIFIED.with(|notified| !notified.replace(true)) {
        for wake in wakes {
            wake.notify();
        }
    }
}

#[cfg(target_os = "windows")]
fn chrome_native_update<T>(update: impl FnOnce(&mut ChromeCaptureLedger) -> T) -> Option<T> {
    let result = CHROME_NATIVE_OWNER.with(|owner| {
        let Ok(mut owner) = owner.try_borrow_mut() else {
            return None;
        };
        if CHROME_NATIVE_FAULT.with(std::cell::Cell::get) {
            owner.ledger.fault();
        }
        let result = update(&mut owner.ledger);
        Some((result, owner.ledger.faulted))
    });
    match result {
        Some((result, faulted)) => {
            if faulted {
                chrome_native_fault();
            }
            Some(result)
        }
        None => {
            chrome_native_fault();
            None
        }
    }
}

#[cfg(target_os = "windows")]
fn chrome_native_loss(registration: &ChromeNativeRegistration) {
    if chrome_native_update(|ledger| ledger.lose(registration.nonce)).unwrap_or(false) {
        registration.wake.notify();
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn chrome_capture_subclass(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _subclass_id: usize,
    ref_data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_CANCELMODE, WM_CAPTURECHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_NCDESTROY,
    };
    let ptr = ref_data as *const ChromeNativeRegistration;
    // SAFETY: successful SetWindowSubclass owns a raw strong reference until
    // confirmed detach or WM_NCDESTROY. A callback retains its own reference
    // across DefSubclassProc, including nested destruction/removal.
    let registration = unsafe {
        std::rc::Rc::increment_strong_count(ptr);
        std::rc::Rc::from_raw(ptr)
    };
    let edge = match message {
        WM_LBUTTONDOWN => Some(true),
        WM_LBUTTONUP => Some(false),
        _ => None,
    };
    let frame = edge.and_then(|down| {
        chrome_native_update(|ledger| ledger.begin(registration.nonce, down)).flatten()
    });
    let normal_up = message == WM_LBUTTONUP
        && unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() } == hwnd
        && chrome_native_update(|ledger| {
            ledger
                .installs
                .iter()
                .find(|s| s.nonce == registration.nonce && s.enabled)
                .and_then(|s| s.native.as_ref())
                .is_some_and(|token| !token.cancelled.get() && !token.finished.get())
        })
        .unwrap_or(false);
    // This saved value scopes nested Up forwarding independently. No mutable
    // registry/RefCell borrow is held while calling winit's native procedure.
    let previous_expected = if message == WM_LBUTTONUP {
        Some(registration.expected_normal_release.replace(normal_up))
    } else {
        None
    };
    if message == WM_CAPTURECHANGED && lparam.0 != hwnd.0 as isize {
        let expected = lparam.0 == 0 && registration.expected_normal_release.replace(false);
        if !expected {
            chrome_native_loss(&registration);
        }
    } else if message == WM_CANCELMODE {
        chrome_native_loss(&registration);
    }
    if message == WM_NCDESTROY {
        let _ = chrome_native_update(|ledger| ledger.retire(registration.nonce));
        if registration.installed.replace(false) {
            if !registration.remove_attempted.replace(true) {
                // Destruction is terminal even if this final removal refuses.
                unsafe {
                    let _ = RemoveWindowSubclass(
                        hwnd,
                        Some(chrome_capture_subclass),
                        CHROME_CAPTURE_SUBCLASS_ID,
                    );
                }
            }
            // SAFETY: release only the installed ownership reference. The
            // callback's temporary strong reference remains live below.
            unsafe {
                drop(std::rc::Rc::from_raw(ptr));
            }
        }
    }
    if CHROME_NATIVE_FAULT.with(std::cell::Cell::get)
        && CHROME_NATIVE_FAULT_NOTIFIED.with(|notified| !notified.replace(true))
    {
        // A reentrant borrow refusal may not have obtained the owner list.
        // The callback's retained registration still provides a safe wake.
        registration.wake.notify();
    }
    // SAFETY: pass the original message unchanged to the next native owner.
    let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    if let Some(previous) = previous_expected {
        registration.expected_normal_release.set(previous);
    }
    if let Some(frame) = frame {
        let _ = chrome_native_update(|ledger| ledger.complete(frame));
    }
    result
}

#[cfg(target_os = "windows")]
fn remove_chrome_registration(registration: &std::rc::Rc<ChromeNativeRegistration>) {
    use windows::Win32::UI::Shell::{GetWindowSubclass, RemoveWindowSubclass};
    let _ = chrome_native_update(|ledger| ledger.retire(registration.nonce));
    if !registration.installed.get() || registration.remove_attempted.replace(true) {
        return;
    }
    // HWND can be reused. Never remove a replacement installation merely
    // because the numeric handle and subclass id happen to match.
    let mut current_data = 0usize;
    let same_registration = unsafe {
        GetWindowSubclass(
            registration.hwnd,
            Some(chrome_capture_subclass),
            CHROME_CAPTURE_SUBCLASS_ID,
            Some(&mut current_data),
        )
    }
    .as_bool()
        && current_data == std::rc::Rc::as_ptr(registration) as usize;
    if !same_registration {
        chrome_native_fault();
        return; // Keep the disabled raw owner valid; replaced refdata may retain it until process exit.
    }
    // SAFETY: all callers run on the window-owning winit UI thread, with a
    // retained registration reference and confirmed original native identity.
    let removed = unsafe {
        RemoveWindowSubclass(
            registration.hwnd,
            Some(chrome_capture_subclass),
            CHROME_CAPTURE_SUBCLASS_ID,
        )
    }
    .as_bool();
    if removed {
        if registration.installed.replace(false) {
            // SAFETY: confirmed removal ends the single raw installed owner.
            unsafe {
                drop(std::rc::Rc::from_raw(std::rc::Rc::as_ptr(registration)));
            }
        }
    } else {
        // Keep the disabled raw owner valid until its destruction callback or process exit. No retry
        // from scope Drop, no free while native code can still call refdata.
        tracing::error!(
            "notification capture observer detach refused; disabled state retained until window destruction"
        );
    }
}

/// Application delivery scope; native callbacks may update tokens, but never
/// borrow the application's state. Retired-window edges do not release a newer
/// HUD press. Faulted current edges still forward unrelated pointer input.
pub(super) struct ChromeAppEdge {
    retired: bool,
    #[cfg(target_os = "windows")]
    previous: Option<bool>,
}

impl ChromeAppEdge {
    pub(super) fn begin(window: winit::window::WindowId, down: bool) -> Self {
        #[cfg(target_os = "windows")]
        {
            let result = CHROME_NATIVE_OWNER.with(|owner| {
                let Ok(mut owner) = owner.try_borrow_mut() else {
                    return None;
                };
                if CHROME_NATIVE_FAULT.with(std::cell::Cell::get) {
                    owner.ledger.fault();
                }
                let permit = owner.ledger.consume(window, down);
                let retired = owner.ledger.last_retired;
                let previous = owner.app_edge.replace(permit);
                Some((retired, previous, owner.ledger.faulted))
            });
            if let Some((retired, previous, faulted)) = result {
                if faulted {
                    chrome_native_fault();
                }
                Self { retired, previous }
            } else {
                chrome_native_fault();
                Self {
                    retired: false,
                    previous: None,
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (window, down);
            Self { retired: false }
        }
    }

    pub(super) fn retired(&self) -> bool {
        self.retired
    }
}

impl Drop for ChromeAppEdge {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        CHROME_NATIVE_OWNER.with(|owner| {
            if let Ok(mut owner) = owner.try_borrow_mut() {
                owner.app_edge = self.previous;
            } else {
                CHROME_NATIVE_FAULT.with(|fault| fault.set(true));
            }
        });
    }
}

fn native_chrome_permits(window: Option<winit::window::WindowId>) -> bool {
    #[cfg(target_os = "windows")]
    {
        if CHROME_NATIVE_FAULT.with(std::cell::Cell::get) {
            return false;
        }
        CHROME_NATIVE_OWNER.with(|owner| {
            owner.try_borrow().ok().is_some_and(|owner| {
                owner
                    .app_edge
                    .unwrap_or_else(|| window.is_some_and(|window| owner.ledger.permit(window)))
            })
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
        true
    }
}

fn native_chrome_take_cancel(window: Option<winit::window::WindowId>) -> bool {
    #[cfg(target_os = "windows")]
    {
        let result = CHROME_NATIVE_OWNER.with(|owner| {
            let Ok(mut owner) = owner.try_borrow_mut() else {
                return None;
            };
            let faulted = CHROME_NATIVE_FAULT.with(std::cell::Cell::get);
            let fault = faulted && !owner.fault_delivered;
            owner.fault_delivered |= faulted;
            Some(window.is_some_and(|window| owner.ledger.take_cancel(window)) || fault)
        });
        result.unwrap_or_else(|| {
            chrome_native_fault();
            true
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
        false
    }
}

fn native_chrome_finish_synthetic(window: Option<winit::window::WindowId>) {
    #[cfg(target_os = "windows")]
    if let Some(window) = window {
        let _ = chrome_native_update(|ledger| ledger.finish_synthetic(window));
    }
    #[cfg(not(target_os = "windows"))]
    let _ = window;
}

/// A temporary construction owner; it contains no Window reference/cycle.
/// Successful publication hands native lifetime to the outer runtime scope.
pub(super) struct ChromeWindowObserver {
    #[cfg(target_os = "windows")]
    registration: Option<std::rc::Rc<ChromeNativeRegistration>>,
    published: bool,
}

impl ChromeWindowObserver {
    pub(super) fn install(
        window: &Window,
        wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    ) -> Self {
        #[cfg(target_os = "windows")]
        {
            use windows::Win32::UI::Shell::SetWindowSubclass;
            let Some(hwnd) = hwnd_for_window(window) else {
                chrome_native_fault();
                return Self {
                    registration: None,
                    published: false,
                };
            };
            // Failed native removals retain their installed raw owners even
            // after ledger debt is empty. Bound those registrations as well.
            let admitted = CHROME_NATIVE_OWNER.with(|owner| {
                let Ok(mut owner) = owner.try_borrow_mut() else {
                    return false;
                };
                owner.registrations.retain(|r| r.strong_count() > 0);
                owner.registrations.len() < ChromeCaptureLedger::MAX_INSTALLATIONS
            });
            if !admitted {
                chrome_native_fault();
                return Self {
                    registration: None,
                    published: false,
                };
            }
            let Some(nonce) = chrome_native_update(|ledger| ledger.install(window.id())).flatten()
            else {
                return Self {
                    registration: None,
                    published: false,
                };
            };
            let registration = std::rc::Rc::new(ChromeNativeRegistration {
                hwnd,
                nonce,
                installed: std::cell::Cell::new(false),
                remove_attempted: std::cell::Cell::new(false),
                expected_normal_release: std::cell::Cell::new(false),
                wake,
            });
            let raw = std::rc::Rc::into_raw(registration.clone());
            // SAFETY: this UI-thread-created window is live, and raw owns one
            // stable strong reference for the installed callback's refdata.
            let installed = unsafe {
                SetWindowSubclass(
                    hwnd,
                    Some(chrome_capture_subclass),
                    CHROME_CAPTURE_SUBCLASS_ID,
                    raw as usize,
                )
            }
            .as_bool();
            if !installed {
                unsafe {
                    drop(std::rc::Rc::from_raw(raw));
                }
                let _ = chrome_native_update(|ledger| ledger.retire(nonce));
                chrome_native_fault();
                return Self {
                    registration: None,
                    published: false,
                };
            }
            registration.installed.set(true);
            let registered = CHROME_NATIVE_OWNER.with(|owner| {
                if let Ok(mut owner) = owner.try_borrow_mut() {
                    owner
                        .registrations
                        .push(std::rc::Rc::downgrade(&registration));
                    true
                } else {
                    false
                }
            });
            if !registered {
                chrome_native_fault();
                remove_chrome_registration(&registration);
                return Self {
                    registration: None,
                    published: false,
                };
            }
            Self {
                registration: Some(registration),
                published: false,
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (window, wake);
            Self { published: false }
        }
    }

    pub(super) fn publish(mut self) {
        self.published = true;
    }
}

impl Drop for ChromeWindowObserver {
    fn drop(&mut self) {
        if !self.published {
            #[cfg(target_os = "windows")]
            if let Some(registration) = &self.registration {
                remove_chrome_registration(registration);
            }
        }
    }
}

pub(super) fn remove_chrome_observer(window: &Window) {
    #[cfg(target_os = "windows")]
    {
        let hwnd = hwnd_for_window(window);
        let registrations = CHROME_NATIVE_OWNER.with(|owner| {
            owner.try_borrow().ok().map(|owner| {
                owner
                    .registrations
                    .iter()
                    .filter_map(std::rc::Weak::upgrade)
                    .filter(|registration| Some(registration.hwnd) == hwnd)
                    .collect::<Vec<_>>()
            })
        });
        if let Some(registrations) = registrations {
            for registration in registrations {
                remove_chrome_registration(&registration);
            }
        } else {
            chrome_native_fault();
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = window;
}

pub(super) fn disable_all_chrome_observers() {
    #[cfg(target_os = "windows")]
    {
        let registrations = CHROME_NATIVE_OWNER.with(|owner| {
            owner.try_borrow().ok().map(|owner| {
                owner
                    .registrations
                    .iter()
                    .filter_map(std::rc::Weak::upgrade)
                    .collect::<Vec<_>>()
            })
        });
        if let Some(registrations) = registrations {
            for registration in registrations {
                let _ = chrome_native_update(|ledger| ledger.retire(registration.nonce));
            }
        } else {
            chrome_native_fault();
        }
    }
}

/// Declared after WinitApp and before run_app: Drop runs before app/window
/// fields on an EventLoopError or startup unwind. Normal return detaches early.
pub(super) struct ChromeObserverScope {
    detached: bool,
    _ui_thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl ChromeObserverScope {
    pub(super) fn new() -> Self {
        Self {
            detached: false,
            _ui_thread: std::marker::PhantomData,
        }
    }
    pub(super) fn detach(&mut self) {
        if self.detached {
            return;
        }
        self.detached = true;
        #[cfg(target_os = "windows")]
        {
            let registrations = CHROME_NATIVE_OWNER.with(|owner| {
                owner.try_borrow().ok().map(|owner| {
                    owner
                        .registrations
                        .iter()
                        .filter_map(std::rc::Weak::upgrade)
                        .collect::<Vec<_>>()
                })
            });
            if let Some(registrations) = registrations {
                for registration in registrations {
                    remove_chrome_registration(&registration);
                }
            } else {
                chrome_native_fault();
            }
        }
    }
}

impl Drop for ChromeObserverScope {
    fn drop(&mut self) {
        self.detach();
    }
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

pub(super) use crate::input_latency::{
    PendingInputLatencySamples, drain_pending_input_latency, record_pending_input_latency,
};

/// Finish local input latency at the actual primary queue submission.
/// The stage duration is an aggregate-success sentinel, not submission evidence.
pub(super) fn drain_windowed_input_latency(
    pending: &PendingInputLatencySamples,
    submitted_batch_ids: &[tze_hud_scene::SceneId],
    gpu_submitted: bool,
) -> Option<(u64, u64, u64)> {
    drain_pending_input_latency(
        pending,
        submitted_batch_ids,
        gpu_submitted.then(Instant::now),
    )
}

pub(super) fn seed_windowed_benchmark_scene(scene: &mut SceneGraph, width: u32, height: u32) {
    use tze_hud_scene::types::HitRegionNode;
    use tze_hud_scene::{Node, NodeData, Rect, Rgba, SceneId, SolidColorNode};

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
    let lease_id = scene.grant_lease(WINDOWED_BENCHMARK_AGENT, 300_000);
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
        self.record_at(telemetry, Instant::now())
    }

    fn record_at(&mut self, telemetry: &tze_hud_telemetry::FrameTelemetry, now: Instant) -> bool {
        if telemetry.stage7_gpu_submit_us == 0 {
            return false;
        }
        self.last_frame_at = now;
        if self.warmup_seen < self.config.warmup_frames {
            self.warmup_seen += 1;
            return false;
        }

        if self.measured_start.is_none() {
            self.measured_start = Some(now);
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
        self.is_stalled_at(timeout, Instant::now())
    }

    fn is_stalled_at(&self, timeout: std::time::Duration, now: Instant) -> bool {
        now.saturating_duration_since(self.last_frame_at) > timeout
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

/// Choose two CPUs without broadening the process's existing allowed mask.
#[cfg(any(target_os = "windows", test))]
fn quiescent_cpu_mask(allowed: usize) -> Option<usize> {
    if allowed.count_ones() < 2 {
        return None;
    }
    let first = 1usize << allowed.trailing_zeros();
    let remaining = allowed & !first;
    Some(first | (1usize << remaining.trailing_zeros()))
}

/// Constrain this opt-in measurement before any runtime initialization.
#[cfg(target_os = "windows")]
pub(super) fn admit_quiescent_cpu_affinity(emit_path: &std::path::Path) -> std::io::Result<()> {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetProcessAffinityMask, GetProcessGroupAffinity, SetProcessAffinityMask,
    };

    // A primary-group mask alone cannot qualify a multi-group process.
    let mut group_count = 1u16;
    let mut group = 0u16;
    let mut process_mask = 0usize;
    let mut system_mask = 0usize;
    // SAFETY: all outputs are writable and the group buffer has group_count
    // entries. The pseudo-handle refers only to this process; no handle is owned.
    unsafe {
        GetProcessGroupAffinity(GetCurrentProcess(), &mut group_count, &mut group)
            .ok()
            .map_err(|error| {
                std::io::Error::other(format!("quiescent CPU group query failed: {error}"))
            })?;
        if group_count != 1 {
            return Err(std::io::Error::other(
                "quiescent measurement requires one CPU group",
            ));
        }
        GetProcessAffinityMask(GetCurrentProcess(), &mut process_mask, &mut system_mask).map_err(
            |error| std::io::Error::other(format!("quiescent CPU affinity query failed: {error}")),
        )?;
    }
    if process_mask & !system_mask != 0 {
        return Err(std::io::Error::other(
            "quiescent process CPU mask is outside the system mask",
        ));
    }
    let selected = quiescent_cpu_mask(process_mask).ok_or_else(|| {
        std::io::Error::other("quiescent measurement requires at least two already-allowed CPUs")
    })?;
    let mut observed = 0usize;
    // SAFETY: selected is a two-bit subset of the observed process/system mask.
    // Both read-back pointers are valid; this changes only this process's threads.
    unsafe {
        if selected != process_mask {
            SetProcessAffinityMask(GetCurrentProcess(), selected).map_err(|error| {
                std::io::Error::other(format!("quiescent CPU affinity setup failed: {error}"))
            })?;
        }
        GetProcessAffinityMask(GetCurrentProcess(), &mut observed, &mut system_mask).map_err(
            |error| {
                std::io::Error::other(format!("quiescent CPU affinity read-back failed: {error}"))
            },
        )?;
    }
    if observed != selected || observed.count_ones() != 2 {
        return Err(std::io::Error::other(format!(
            "quiescent CPU affinity read-back mismatch: expected {selected:#x}, got {observed:#x}"
        )));
    }
    tracing::info!(
        cpu_mask = observed,
        logical_cpus = observed.count_ones(),
        "quiescent two-CPU affinity admitted before runtime initialization"
    );
    let admission = format!(
        "quiescent-admission pid={} cpu_mask={observed:#x} logical_cpus={} phase=before_runtime_init",
        std::process::id(),
        observed.count_ones()
    );
    if let Some(parent) = emit_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let admission_path = emit_path.with_extension("admission.log");
    std::fs::write(admission_path, format!("{admission}\n"))?;
    eprintln!("{admission}");
    Ok(())
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

    /// Drop keyboard focus from a tile that lease revocation, expiry, or a
    /// portal clear removed. A composer on that tile is blurred and its local
    /// echo cleared; otherwise the compositor keeps blinking a caret for a
    /// composer that no longer exists, presenting a frame per blink on an
    /// otherwise idle HUD (hud-oyo7k).
    /// Non-blocking: skips this frame if the scene lock is busy.
    pub(super) fn clear_focus_on_removed_tile(&mut self) {
        let composer_lost = {
            let Ok(state) = self.state.shared_state.try_lock() else {
                return;
            };
            let Ok(mut scene) = state.scene.try_lock() else {
                return;
            };
            let Some(tab_id) = scene.active_tab else {
                return;
            };
            let was_active = self.state.input_processor.is_composer_active();
            self.state.input_processor.clear_focus_on_missing_tile(
                &mut self.state.focus_manager,
                &mut scene,
                tab_id,
            );
            was_active && !self.state.input_processor.is_composer_active()
        };
        if composer_lost {
            self.clear_local_composer_echo();
            self.state.composer_pointer_drag_anchor = None;
        }
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

    /// Publish the tile under the pointer to the compositor so it shows that
    /// tile's viewer close button (hud-jm8nq.11).
    ///
    /// Resolved from the polled cursor and the lock-free hit-test snapshot, so
    /// it works in passthrough overlay mode (where the OS delivers no pointer
    /// events and `refresh_cursor_position_from_os` supplies the cursor) and
    /// needs no scene lock. Latest-wins: the compositor renders only when the
    /// target changes, so an idle HUD stays idle.
    pub(super) fn push_tile_close_hover(&self) {
        let target = self.close_hover_target();
        if let Ok(mut slot) = self.state.tile_close_hover_state.lock() {
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

    /// A winit left-button press or release on `window_id` (`secondary` = the
    /// secondary overlay index, `None` for the primary).
    pub(super) fn handle_left_button(
        &mut self,
        pressed: bool,
        window_id: winit::window::WindowId,
        secondary: Option<usize>,
    ) {
        if pressed {
            if self.state.left_button_down {
                self.enqueue_pointer_event(PointerEventKind::Up);
                end_os_mouse_capture();
            }
            self.state.left_button_down = true;
            self.state.press_window = Some(window_id);
            let window = match secondary {
                Some(i) => Some(&self.state.secondaries[i].window),
                None => self.state.window.as_ref(),
            };
            if let Some(window) = window {
                focus_window_for_text_input(window);
                begin_os_mouse_capture(window);
            }
            self.update_overlay_cursor_hittest();
            self.enqueue_pointer_event(PointerEventKind::Down);
            return;
        }
        // The press already ended: `synthesize_left_release_if_physically_up`
        // saw the button up before this event arrived and sent the Up. A
        // second Up would repeat the release (hud-z21e9).
        if !self.state.left_button_down {
            return;
        }
        // If the context menu is showing, a release on its Reset button
        // triggers the reset; anywhere else dismisses the menu.
        self.handle_left_click_with_context_menu();
        self.enqueue_pointer_event(PointerEventKind::Up);
        self.state.left_button_down = false;
        self.state.press_window = None;
        end_os_mouse_capture();
        self.update_overlay_cursor_hittest();
    }

    /// Disarm only runtime chrome. This is not an Up and cannot activate a
    /// callback or human X; ordinary button/capture ownership remains intact.
    pub(super) fn cancel_chrome_feedback(&mut self) {
        self.state.zone_press = None;
        self.state.input_processor.chrome_feedback_cancel_pending = true;
    }

    pub(super) fn drain_chrome_capture_cancellation(&mut self) {
        if native_chrome_take_cancel(self.state.press_window) {
            self.cancel_chrome_feedback();
        }
    }

    pub(super) fn synthesize_left_release_if_physically_up(&mut self) -> bool {
        self.drain_chrome_capture_cancellation();
        if self.state.left_button_down
            && matches!(left_mouse_button_is_physically_down(), Some(false))
        {
            self.release_left_press();
            return true;
        }
        false
    }

    /// End the current left press with an Up, as if its release arrived.
    pub(super) fn release_left_press(&mut self) {
        self.drain_chrome_capture_cancellation();
        self.enqueue_pointer_event(PointerEventKind::Up);
        native_chrome_finish_synthetic(self.state.press_window);
        self.state.left_button_down = false;
        self.state.press_window = None;
        end_os_mouse_capture();
        self.update_overlay_cursor_hittest();
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
        // Settle the old terminal before admitting any newer drag. If the lock
        // is still busy, retain the original Up instead of retargeting it to a
        // new Down or moving the already-released gesture.
        if self
            .state
            .primary_drag_gesture
            .as_ref()
            .is_some_and(|gesture| gesture.terminal.is_some())
        {
            self.drain_pending_drag_terminal();
            if self
                .state
                .primary_drag_gesture
                .as_ref()
                .is_some_and(|gesture| gesture.terminal.is_some())
            {
                self.schedule_shared_scene_availability_wake();
                return;
            }
        }
        let original_window_is_live = self.primary_drag_window_is_live();
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
        // Terminal lease transitions from viewer dismisses (hover close button),
        // published to the owning sessions once the scene lock is released.
        let mut viewer_dismiss_expiries: Vec<tze_hud_scene::types::LeaseExpiry> = Vec::new();
        let mut pending_action_wakes = Vec::new();
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
        let mut pointer_processed = false;
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
                pointer_processed = true;
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
                // Callback and human-X events enter their publisher's retained
                // MCP input lane; notification tokens are delivered unlocked.
                //
                // Button semantics (hud-z21e9): a release activates an
                // affordance only when the press began on that same
                // affordance, so a release that lands elsewhere (the cursor
                // moved, or a press that started off the button) never fires
                // an action the user did not press.
                if pointer_event.kind == PointerEventKind::Down {
                    self.state.zone_press = match &result.hit {
                        HitResult::ZoneInteraction {
                            interaction_id,
                            kind,
                            ..
                        } => {
                            let notification = matches!(
                                kind,
                                ZoneInteractionKind::Dismiss | ZoneInteractionKind::Action { .. }
                            );
                            let allowed = self.state.window.is_none()
                                || native_chrome_permits(self.state.press_window);
                            (!notification || allowed).then(|| interaction_id.clone())
                        }
                        _ => None,
                    };
                }
                if pointer_event.kind == PointerEventKind::Up {
                    let pressed = self.state.zone_press.take();
                    if let HitResult::ZoneInteraction {
                        ref zone_name,
                        published_at_wall_us,
                        ref publisher_namespace,
                        ref kind,
                        ref interaction_id,
                    } = result.hit
                        && pressed.as_deref() == Some(interaction_id.as_str())
                        && (!matches!(
                            kind,
                            ZoneInteractionKind::Dismiss | ZoneInteractionKind::Action { .. }
                        ) || self.state.window.is_none()
                            || native_chrome_permits(self.state.press_window))
                    {
                        match kind {
                            ZoneInteractionKind::Dismiss => {
                                let removed = scene.dismiss_notification(
                                    zone_name,
                                    published_at_wall_us,
                                    publisher_namespace,
                                );
                                if removed {
                                    pending_action_wakes.push(scene.push_pending_action(
                                        tze_hud_scene::PendingAction {
                                            publisher_namespace: publisher_namespace.clone(),
                                            zone_name: zone_name.clone(),
                                            kind: tze_hud_scene::graph::PendingActionKind::Dismiss,
                                        },
                                    ));
                                }
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
                                pending_action_wakes.push(scene.push_pending_action(
                                    tze_hud_scene::PendingAction {
                                        publisher_namespace: publisher_namespace.clone(),
                                        zone_name: zone_name.clone(),
                                        kind: tze_hud_scene::graph::PendingActionKind::Callback(
                                            callback_id.clone(),
                                        ),
                                    },
                                ));
                                // An activated notification is answered: dismiss
                                // it locally, as the × does, so the click has an
                                // immediate visible result and cannot repeat
                                // (hud-paslt).
                                let removed = scene.dismiss_notification(
                                    zone_name,
                                    published_at_wall_us,
                                    publisher_namespace,
                                );
                                tracing::debug!(
                                    zone = %zone_name,
                                    published_at_wall_us,
                                    publisher = %publisher_namespace,
                                    %callback_id,
                                    removed,
                                    "zone action: callback queued for agent delivery, notification dismissed"
                                );
                            }
                            ZoneInteractionKind::DragHandle { .. } => {
                                // Handled by the drag state machine below — not here.
                            }
                            ZoneInteractionKind::DismissTile { tile_id } => {
                                // Human override: reclaim the tile's lease on
                                // this thread's own scene lock, so the tile is
                                // gone before the next frame. The agent learns
                                // of it via `Reclaimed{OVERRIDE}`, published
                                // after the lock is released.
                                let dismissed = self
                                    .state
                                    .portal_projection_driver
                                    .viewer_dismiss_tile(&mut scene, *tile_id);
                                tracing::debug!(
                                    tile_id = ?tile_id,
                                    reclaimed = dismissed.expiry.is_some(),
                                    "viewer dismiss: tile close button pressed"
                                );
                                viewer_dismiss_expiries.extend(dismissed.expiry);
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
                if self
                    .state
                    .primary_drag_gesture
                    .as_ref()
                    .is_some_and(|gesture| {
                        !original_window_is_live || !gesture.target_is_live(&scene)
                    })
                {
                    if let Some(gesture) = self.state.primary_drag_gesture.take()
                        && self
                            .state
                            .input_processor
                            .drag_states
                            .get(&0)
                            .is_some_and(|drag| gesture.matches(drag))
                    {
                        self.state.input_processor.drag_states.remove(&0);
                    }
                    reconcile_drag_feedback(&self.state.input_processor, &mut scene);
                }
                drag_released = apply_drag_handle_pointer_event(
                    &mut self.state.input_processor,
                    &pointer_event,
                    &result.hit,
                    &mut scene,
                    display_w,
                    display_h,
                );
                if pointer_event.kind == PointerEventKind::Down {
                    self.state.primary_drag_gesture = self
                        .state
                        .input_processor
                        .drag_states
                        .get(&0)
                        .and_then(|drag| {
                            scene
                                .tiles
                                .get(&drag.element_id)
                                .map(|tile| PrimaryDragGesture {
                                    element_id: drag.element_id,
                                    lease_id: tile.lease_id,
                                    feedback_target: drag_feedback_target(&scene, drag.element_id),
                                    interaction_id: drag.interaction_id.clone(),
                                    press_start: drag.press_start,
                                    window_id: self.state.press_window,
                                    window_allocation: self
                                        .primary_drag_window_allocation(self.state.press_window),
                                    terminal: None,
                                })
                        });
                } else if !self.state.input_processor.drag_states.contains_key(&0) {
                    self.state.primary_drag_gesture = None;
                }

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

        // The accepted input is committed and both scene/state guards are gone.
        for notifier in pending_action_wakes {
            notifier.notify();
        }

        // A press the scene lock never saw cannot arm a zone affordance.
        if !pointer_processed && pointer_event.kind == PointerEventKind::Down {
            self.state.zone_press = None;
        }

        if !pointer_processed
            && pointer_event.kind == PointerEventKind::Up
            && let Some(gesture) = self.state.primary_drag_gesture.as_mut()
        {
            gesture.terminal = Some(pointer_event);
            self.schedule_shared_scene_availability_wake();
        }

        // ── Post-lock: persist geometry override after drag release ───────────
        // The drag state machine has already updated tile.bounds for live visual
        // feedback.  Here we also write the geometry_override to the element store
        // (durable) and broadcast an ElementRepositionedEvent so subscribers know
        // the tile moved.
        if let Some(released) = drag_released {
            self.persist_drag_release(released);
        }

        // ── Post-lock: tell the dismissed tiles' agents ───────────────────────
        super::publish_lease_expiries(
            self.state.lease_expirations.as_ref(),
            viewer_dismiss_expiries,
        );

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
            for member in &outcome.members {
                dispatch_portal_geometry_event(
                    &self.state.element_repositioned_tx,
                    member.tile_id,
                    &member.snapshot,
                    outcome.display_w,
                    outcome.display_h,
                );
                // §6b.4 producer wiring (hud-npq6g): the next portal render
                // sizes against live pointer-affordance geometry (same path as
                // the hotkey resize wiring).
                self.state
                    .portal_projection_driver
                    .push_geometry_snapshot_for_tile(member.tile_id, member.snapshot);
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

            // Secondary overlays first: they never carry the primary's
            // present accounting.
            let secondary_presented = self.present_secondaries();
            if let Some(surface) = &self.state.window_surface {
                // Present under the same mutex that guards pending swapchain
                // ownership so acquire/present cannot interleave into a
                // double-acquire validation error on some backends.
                if surface.present_pending_texture() {
                    self.state.wake.counters().record_present();
                    if let Some(measurement) = self.state.quiescent_efficiency.as_mut() {
                        measurement.record_first_present(Instant::now());
                    }
                } else if !secondary_presented {
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
        use crate::input_latency::record_committed_input_response;

        let pending = Arc::new(StdMutex::new(VecDeque::new()));
        let started = Instant::now() - std::time::Duration::from_millis(3);
        let scene_commit_at = Instant::now() - std::time::Duration::from_millis(1);
        let batch_id = tze_hud_scene::SceneId::new();
        record_pending_input_latency(&pending, started, 125);
        record_committed_input_response(&pending, started, 125, batch_id, scene_commit_at);
        // Repeating an outstanding response does not append another sample.
        record_committed_input_response(&pending, started, 999, batch_id, scene_commit_at);
        assert!(drain_pending_input_latency(&pending, &[batch_id], None).is_none());
        let mut failed_telemetry = tze_hud_telemetry::FrameTelemetry::new(1);
        failed_telemetry.stage7_gpu_submit_us = 17;
        let failed_outcome = tze_hud_compositor::renderer::frame::WindowedPresentOutcome {
            telemetry: failed_telemetry,
            surface_acquired: true,
            gpu_submitted: false,
        };
        // A duration must never substitute for the actual submission result.
        assert!(
            drain_windowed_input_latency(&pending, &[batch_id], failed_outcome.gpu_submitted)
                .is_none()
        );
        assert_eq!(failed_outcome.telemetry.stage7_gpu_submit_us, 17);
        assert_eq!(
            pending.lock().unwrap().len(),
            2,
            "failed submit retains samples"
        );
        let unrelated = tze_hud_scene::SceneId::new();
        // Inject the production post-submit present/poll failure shape; this is
        // outcome-to-drain coverage, not a GPU panic reproduction.
        let submitted_outcome = tze_hud_compositor::renderer::frame::WindowedPresentOutcome {
            telemetry: tze_hud_telemetry::FrameTelemetry::new(2),
            surface_acquired: true,
            gpu_submitted: true,
        };
        let gpu_submitted = submitted_outcome.gpu_submitted;
        let skipped_stage_telemetry = submitted_outcome.telemetry;
        assert_eq!(skipped_stage_telemetry.stage7_gpu_submit_us, 0);
        assert_eq!(
            drain_windowed_input_latency(&pending, &[unrelated], gpu_submitted),
            Some((125, 0, 0)),
            "anonymous input is local ack only; an unrelated batch is not a response"
        );
        assert_eq!(pending.lock().unwrap().len(), 1);

        let (local_ack, scene_commit, next_present) =
            drain_pending_input_latency(&pending, &[batch_id], Some(Instant::now()))
                .expect("sample drains");

        assert_eq!(local_ack, 125);
        assert!(scene_commit >= 2_000);
        assert!(next_present >= 3_000);
        assert!(scene_commit < next_present);
        assert!(
            drain_pending_input_latency(&pending, &[batch_id], Some(Instant::now())).is_none(),
            "sample should be consumed exactly once"
        );
        // An invalid commit ordering is refused, rather than becoming a zero sample.
        record_committed_input_response(&pending, Instant::now(), 125, batch_id, started);
        assert!(drain_pending_input_latency(&pending, &[batch_id], Some(Instant::now())).is_none());
    }

    #[test]
    fn pending_input_latency_clamps_sub_microsecond_local_ack_to_nonzero_sample() {
        let pending = Arc::new(StdMutex::new(VecDeque::new()));
        let started = Instant::now() - std::time::Duration::from_millis(1);
        record_pending_input_latency(&pending, started, 0);

        let (local_ack, _, _) = drain_pending_input_latency(&pending, &[], Some(Instant::now()))
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
        for (allowed, expected) in [
            (0, None),
            (1, None),
            (8, None),
            (3, Some(3)),
            (0x50, Some(0x50)),
            (0xf, Some(3)),
            (usize::MAX, Some(3)),
        ] {
            let selected = quiescent_cpu_mask(allowed);
            assert_eq!(selected, expected);
            if let Some(mask) = selected {
                assert_eq!(mask.count_ones(), 2);
                assert_eq!(mask & !allowed, 0, "admission must never widen affinity");
            }
        }

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
        assert!(
            windowed_frame_needs_render(false, false, true, false, false, false),
            "normal sessions render while the compositor reports a transition"
        );
        assert!(
            !windowed_frame_needs_render(false, false, false, false, false, false),
            "the same production consumer sleeps after the transition lands"
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
        let created_at = state.last_frame_at;
        let timeout = Duration::from_millis(1);
        assert!(
            !state.is_stalled_at(timeout, created_at + timeout),
            "watchdog must remain active exactly at the timeout boundary"
        );
        assert!(
            state.is_stalled_at(timeout, created_at + timeout + Duration::from_millis(1)),
            "watchdog must fire when no frame recorded past the timeout"
        );
    }

    #[test]
    fn benchmark_watchdog_does_not_fire_immediately_after_record() {
        let mut state = make_benchmark_state(0, 10);
        let created_at = state.last_frame_at;
        let timeout = Duration::from_millis(100);
        let recorded_at = created_at + Duration::from_millis(200);
        assert!(state.is_stalled_at(timeout, recorded_at));

        let mut telem = tze_hud_telemetry::FrameTelemetry::new(1);
        telem.frame_time_us = 8_000;
        telem.stage7_gpu_submit_us = 1;
        state.record_at(&telem, recorded_at);
        assert!(
            !state.is_stalled_at(timeout, recorded_at),
            "watchdog must not fire immediately after a frame is recorded"
        );
        assert!(
            !state.is_stalled_at(timeout, recorded_at + timeout),
            "a successful record must refresh the exact timeout boundary"
        );
        assert!(
            !state.is_stalled_at(timeout, created_at),
            "an earlier injected timestamp must not panic or spuriously stall"
        );

        telem.stage7_gpu_submit_us = 0;
        let skipped_at = recorded_at + timeout + Duration::from_millis(1);
        assert!(!state.record_at(&telem, skipped_at));
        assert!(
            state.is_stalled_at(timeout, skipped_at),
            "a skipped submit must not refresh watchdog progress"
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
