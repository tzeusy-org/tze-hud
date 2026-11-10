//! All HWND, menu, HICON and WTS ownership stays on display-change-watch.
//! No callback waits for a scene lock, an agent, or the main window thread.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{Value, json};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTSDisconnected,
    WTSFreeMemory, WTSINFOEXW, WTSQuerySessionInformationW, WTSRegisterSessionNotification,
    WTSSessionInfoEx, WTSUnRegisterSessionNotification,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, DestroyWindow,
    GetCursorPos, HICON, HMENU, ICONINFO, MF_STRING, PostMessageW, RegisterWindowMessageW,
    SM_CXSMICON, SetForegroundWindow, SetMenuDefaultItem, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, WM_APP, WM_CONTEXTMENU, WM_DPICHANGED, WM_NULL, WM_SETTINGCHANGE,
    WM_WTSSESSION_CHANGE,
};
use windows::core::{PCWSTR, PWSTR, w};

use super::{GlyphColors, GlyphState, Inputs, Presentation, SessionState};
use crate::shell::control_panel::{ControlPanelHandle, PanelEvent, TrayCommand};

const ICON_ID: u32 = 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const CALLBACK: u32 = WM_APP + 31;
const REFRESH: u32 = WM_APP + 32;
const STOP: u32 = WM_APP + 33;

#[derive(Clone, PartialEq, Eq)]
struct Snapshot {
    view: Presentation,
    colors: GlyphColors,
}

/// Latest-only delivery. A pre-window update is retained and reconciled at
/// attach. Posting never increments a HUD/compositor wake generation.
pub(in crate::windowed) struct Bridge {
    hwnd: AtomicIsize,
    pending: AtomicBool,
    latest: RwLock<Snapshot>,
    registered: AtomicBool,
    menu_open: AtomicBool,
    stopping: AtomicBool,
    session_locked: AtomicBool,
    panel: Option<ControlPanelHandle>,
    quit: crate::operator::handoff::Quit,
}

static BRIDGE: OnceLock<Arc<Bridge>> = OnceLock::new();

pub(in crate::windowed) fn bridge() -> Option<Arc<Bridge>> {
    BRIDGE.get().cloned()
}

pub(crate) fn status() -> Value {
    let Some(bridge) = BRIDGE.get() else {
        return json!({"registered": false, "glyph": "normal", "tooltip": "No questions\nNo agents"});
    };
    let latest = bridge.latest.read().unwrap_or_else(|e| e.into_inner());
    json!({
        "registered": bridge.registered.load(Ordering::Acquire),
        "glyph": latest.view.glyph,
        "tooltip": latest.view.tooltip,
        "all_hidden": latest.view.all_hidden,
        "menu_open": bridge.menu_open.load(Ordering::Acquire),
        // Admin-only native identity for the tier-W real-input harness.
        "hwnd": bridge.hwnd.load(Ordering::Acquire),
        "icon_id": ICON_ID,
    })
}

pub(crate) fn session_locked() -> bool {
    BRIDGE
        .get()
        .is_some_and(|b| b.session_locked.load(Ordering::Acquire))
}

impl Bridge {
    pub(in crate::windowed) fn install(
        panel: Option<ControlPanelHandle>,
        colors: GlyphColors,
        quit: crate::operator::handoff::Quit,
    ) -> Arc<Self> {
        let bridge = Arc::new(Self {
            hwnd: AtomicIsize::new(0),
            pending: AtomicBool::new(false),
            latest: RwLock::new(Snapshot {
                view: Presentation::from(&Inputs::default()),
                colors,
            }),
            registered: AtomicBool::new(false),
            menu_open: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            session_locked: AtomicBool::new(false),
            panel,
            quit,
        });
        BRIDGE
            .set(Arc::clone(&bridge))
            .unwrap_or_else(|_| panic!("shell presence installed twice"));
        bridge
    }

    pub(in crate::windowed) fn publish(&self, input: Inputs, colors: GlyphColors) {
        let next = Snapshot {
            view: Presentation::from(&input),
            colors,
        };
        let mut latest = self.latest.write().unwrap_or_else(|e| e.into_inner());
        if *latest == next {
            return;
        }
        *latest = next;
        drop(latest);
        self.post_refresh();
    }

    fn post_refresh(&self) {
        let hwnd = self.hwnd.load(Ordering::Acquire);
        if hwnd != 0 && !self.pending.swap(true, Ordering::AcqRel) {
            // SAFETY: an integer HWND is used only to post; the owner clears it
            // before destroying the window. Failure leaves no pending debt.
            if unsafe { PostMessageW(HWND(hwnd as _), REFRESH, WPARAM(0), LPARAM(0)) }.is_err() {
                self.pending.store(false, Ordering::Release);
            }
        }
    }

    pub(in crate::windowed) fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        let hwnd = self.hwnd.load(Ordering::Acquire);
        if hwnd != 0 {
            // SAFETY: no pointer payload, asynchronous owner-thread teardown.
            let _ = unsafe { PostMessageW(HWND(hwnd as _), STOP, WPARAM(0), LPARAM(0)) };
        }
    }

    fn command(&self, command: TrayCommand) {
        if let Some(panel) = &self.panel {
            panel.send(PanelEvent::Tray(command));
        } else if command == TrayCommand::Quit {
            (self.quit)();
        }
    }

    pub(in crate::windowed) fn observe(
        self: &Arc<Self>,
        runtime: &tokio::runtime::Handle,
        deps: crate::shell::control_panel::ControlPanelDeps,
        changes: Arc<tokio::sync::Notify>,
        shutdown: crate::threads::ShutdownToken,
    ) {
        let bridge = Arc::clone(self);
        runtime.spawn(async move {
            let mut shutdown_rx = shutdown.subscribe();
            loop {
                if shutdown.is_triggered() { bridge.stop(); break; }
                let agents = match &deps.agents {
                    Some(agents) => agents.active_names().await,
                    None => Vec::new(),
                };
                let (hidden, visible_questions) = {
                    let scene = deps.scene.lock().await;
                    let now = scene.now_wall_us();
                    let questions = scene.zone_registry.active_publishes.values().flatten()
                        .filter(|record| record.expires_at_wall_us.is_none_or(|end| end > now)
                            && !scene.overlay.notification_choices.contains_key(&record.publication_id)
                            && matches!(&record.content, tze_hud_scene::ZoneContent::Notification(payload)
                                if payload.actions.iter().any(|action| action.callback_id.is_some() && action.validate().is_ok())))
                        .count();
                    (scene.owner_hidden(), questions)
                };
                let colors = deps.appearance.as_ref()
                    .map(|appearance| GlyphColors::from_tokens(&appearance.live.resolved()))
                    .unwrap_or_else(|| bridge.latest.read().unwrap_or_else(|e| e.into_inner()).colors.clone());
                bridge.publish(Inputs {
                    paused: deps.pause.is_paused(), hidden, visible_questions,
                    held_questions: 0, agents,
                }, colors);
                tokio::select! {
                    _ = changes.notified() => {},
                    _ = shutdown_rx.recv() => { bridge.stop(); break; }
                }
            }
        });
    }
}

struct Icon(HICON);
impl Drop for Icon {
    fn drop(&mut self) {
        // SAFETY: this exclusively owned CreateIconIndirect result is no longer
        // used by the shell (NotifyIcon copied it) or an in-flight update.
        let _ = unsafe { DestroyIcon(self.0) };
    }
}

struct Menu(HMENU);
impl Drop for Menu {
    fn drop(&mut self) {
        // SAFETY: this menu's synchronous modal loop has finished.
        let _ = unsafe { DestroyMenu(self.0) };
    }
}

/// Interior state is borrowed only around local copies. In particular no
/// RefCell borrow crosses TrackPopupMenu's reentrant modal message loop.
pub(in crate::windowed) struct Host {
    hwnd: HWND,
    bridge: Arc<Bridge>,
    taskbar_created: u32,
    session: Cell<SessionState>,
    wts_registered: bool,
    applied: RefCell<Option<(Snapshot, bool, i32)>>,
    icon: RefCell<Option<Icon>>,
    stopped: Cell<bool>,
}

impl Host {
    pub(in crate::windowed) fn new(hwnd: HWND, bridge: Arc<Bridge>) -> Self {
        // SAFETY: registration and query belong to this live owner-thread HWND.
        let wts_registered =
            unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) }
                .inspect_err(|e| tracing::warn!(%e, "Windows session notifications unavailable"))
                .is_ok();
        let session = initial_session().unwrap_or_default();
        bridge
            .session_locked
            .store(session.held(), Ordering::Release);
        bridge.hwnd.store(hwnd.0 as isize, Ordering::Release);
        let host = Self {
            hwnd,
            bridge,
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            session: Cell::new(session),
            wts_registered,
            applied: RefCell::new(None),
            icon: RefCell::new(None),
            stopped: Cell::new(false),
        };
        if host.bridge.stopping.load(Ordering::Acquire) {
            host.stop();
            let _ = unsafe { DestroyWindow(hwnd) };
        } else {
            host.update(true);
        }
        host
    }

    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            ..Default::default()
        }
    }

    fn update(&self, add: bool) {
        if self.stopped.get() {
            return;
        }
        let next = self
            .bridge
            .latest
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let light = light_taskbar();
        // The watcher is a primary-taskbar window. Windows reports the actual
        // small-icon metric rather than a HUD zoom or a hardcoded pixel size.
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(self.hwnd)) };
        if !add && self.applied.borrow().as_ref() == Some(&(next.clone(), light, size)) {
            return;
        }
        // A failed add waits for the next TaskbarCreated, never a retry timer.
        if !add && !self.bridge.registered.load(Ordering::Acquire) {
            return;
        }
        let pixels_changed =
            self.applied
                .borrow()
                .as_ref()
                .is_none_or(|(old, old_light, old_size)| {
                    old.colors != next.colors
                        || old.view.glyph != next.view.glyph
                        || *old_light != light
                        || *old_size != size
                });
        let replacement = if pixels_changed {
            match icon(&next.colors.svg(next.view.glyph, light), size) {
                Ok(icon) => Some(icon),
                Err(error) => {
                    tracing::error!(%error, "tray glyph rasterization failed");
                    return;
                }
            }
        } else {
            None
        };
        let mut data = self.data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = CALLBACK;
        data.hIcon = replacement
            .as_ref()
            .map(|i| i.0)
            .or_else(|| self.icon.borrow().as_ref().map(|i| i.0))
            .unwrap_or_default();
        for (out, unit) in data.szTip.iter_mut().zip(next.view.tooltip.encode_utf16()) {
            *out = unit;
        }
        // SAFETY: fully initialized structure; szTip's trailing NUL is retained.
        let ok =
            unsafe { Shell_NotifyIconW(if add { NIM_ADD } else { NIM_MODIFY }, &data) }.as_bool();
        if !ok {
            self.bridge.registered.store(false, Ordering::Release);
            tracing::warn!(
                add,
                "tray registration/update failed; retry on TaskbarCreated"
            );
            return;
        }
        if add {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool() {
                let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &self.data()) };
                self.bridge.registered.store(false, Ordering::Release);
                tracing::warn!("tray VERSION_4 registration failed; retry on TaskbarCreated");
                return;
            }
        }
        if let Some(icon) = replacement {
            *self.icon.borrow_mut() = Some(icon);
        }
        *self.applied.borrow_mut() = Some((next, light, size));
        self.bridge.registered.store(true, Ordering::Release);
    }

    pub(in crate::windowed) fn message(
        &self,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        match message {
            REFRESH => {
                self.bridge.pending.store(false, Ordering::Release);
                self.update(false);
            }
            STOP => {
                self.stop();
                // SAFETY: stop clears the cross-thread HWND before destruction.
                let _ = unsafe { DestroyWindow(self.hwnd) };
            }
            WM_WTSSESSION_CHANGE => {
                let mut session = self.session.get();
                session.event(wparam.0 as u32);
                self.session.set(session);
                self.bridge
                    .session_locked
                    .store(session.held(), Ordering::Release);
            }
            WM_SETTINGCHANGE | WM_DPICHANGED => self.update(false),
            CALLBACK => {
                // VERSION_4: event in LOWORD(lParam), icon id in HIWORD.
                let event = lparam.0 as u32 & 0xffff;
                let id = (lparam.0 as u32 >> 16) & 0xffff;
                if id != ICON_ID {
                    return Some(LRESULT(0));
                }
                match event {
                    NIN_SELECT | NIN_KEYSELECT => self.bridge.command(TrayCommand::TogglePanel), // NIN_KEYSELECT
                    WM_CONTEXTMENU => self.menu(wparam),
                    _ => {}
                }
            }
            n if self.taskbar_created != 0 && n == self.taskbar_created => {
                self.bridge.registered.store(false, Ordering::Release);
                self.update(true);
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }

    fn menu(&self, position: WPARAM) {
        let view = self
            .bridge
            .latest
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .view
            .clone();
        let menu = match unsafe { CreatePopupMenu() } {
            Ok(menu) => Menu(menu),
            Err(error) => {
                tracing::warn!(%error, "tray menu unavailable");
                return;
            }
        };
        let labels = [
            "&Open panel",
            if view.glyph == GlyphState::Paused {
                "&Resume agents"
            } else {
                "&Pause agents"
            },
            if view.all_hidden {
                "&Show all"
            } else {
                "&Hide all"
            },
            "&Quit",
        ];
        for (index, label) in labels.into_iter().enumerate() {
            let wide: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
            if let Err(error) =
                unsafe { AppendMenuW(menu.0, MF_STRING, index + 1, PCWSTR(wide.as_ptr())) }
            {
                tracing::warn!(%error, "tray menu item unavailable");
                return;
            }
        }
        let mut point = windows::Win32::Foundation::POINT {
            x: position.0 as u16 as i16 as i32,
            y: (position.0 >> 16) as u16 as i16 as i32,
        };
        if point.x == -1 && point.y == -1 {
            let _ = unsafe { GetCursorPos(&mut point) };
        }
        // Notification menus require foreground ownership and the trailing
        // WM_NULL for reliable dismiss/reopen. TrackPopupMenu dispatches this
        // same thread's display/session messages while it is open.
        self.bridge.menu_open.store(true, Ordering::Release);
        let id = unsafe {
            let _ = SetMenuDefaultItem(menu.0, 1, 0);
            let _ = SetForegroundWindow(self.hwnd);
            let id = TrackPopupMenu(
                menu.0,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                self.hwnd,
                None,
            )
            .0 as u32;
            let _ = PostMessageW(self.hwnd, WM_NULL, WPARAM(0), LPARAM(0));
            id
        };
        self.bridge.menu_open.store(false, Ordering::Release);
        if let Some(command) = TrayCommand::from_menu_id(id, &view) {
            self.bridge.command(command);
        }
    }

    pub(in crate::windowed) fn stop(&self) {
        if self.stopped.replace(true) {
            return;
        }
        self.bridge.hwnd.store(0, Ordering::Release);
        self.bridge.registered.store(false, Ordering::Release);
        self.bridge.session_locked.store(false, Ordering::Release);
        // SAFETY: all registrations and resources belong to this thread/HWND.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
            if self.wts_registered {
                let _ = WTSUnRegisterSessionNotification(self.hwnd);
            }
        }
        self.icon.borrow_mut().take();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
    }
}

fn light_taskbar() -> bool {
    let mut value = 0u32;
    let mut bytes = std::mem::size_of_val(&value) as u32;
    // SAFETY: bounded DWORD read from the documented per-user theme value.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut bytes),
        )
    };
    result.is_ok() && value != 0
}

fn initial_session() -> Option<SessionState> {
    let mut data = PWSTR::null();
    let mut bytes = 0;
    // SAFETY: WTS allocates this query result; it is always freed below.
    if let Err(error) = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut data,
            &mut bytes,
        )
    } {
        tracing::warn!(%error, "initial Windows session state unavailable");
        return None;
    }
    let state = if !data.is_null() && bytes as usize >= std::mem::size_of::<WTSINFOEXW>() {
        let info = unsafe { &*data.0.cast::<WTSINFOEXW>() };
        if info.Level == 1 {
            let level = unsafe { info.Data.WTSInfoExLevel1 };
            Some(SessionState {
                locked: level.SessionFlags == 0,
                disconnected: level.SessionState == WTSDisconnected,
            })
        } else {
            None
        }
    } else {
        None
    };
    unsafe { WTSFreeMemory(data.0.cast()) };
    state
}

fn icon(svg: &str, size: i32) -> Result<Icon, String> {
    let size = u32::try_from(size)
        .ok()
        .filter(|n| *n > 0)
        .ok_or("invalid small-icon metric")?;
    let mut pixmap = super::rasterize(svg, size)?;
    // tiny-skia is premultiplied RGBA; a top-down Windows 32bpp DIB is
    // premultiplied BGRA. Preserve alpha while swapping only R and B.
    for pixel in pixmap.data_mut().chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let bitmap_info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    // SAFETY: matching dimensions/format and exactly size²*4 bytes. Both GDI
    // bitmaps are temporary; CreateIconIndirect copies them on success.
    unsafe {
        let color = CreateDIBSection(None, &bitmap_info, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|e| e.to_string())?;
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, None);
        if mask.is_invalid() || bits.is_null() {
            let _ = DeleteObject(color);
            if !mask.is_invalid() {
                let _ = DeleteObject(mask);
            }
            return Err("tray bitmap allocation failed".into());
        }
        std::ptr::copy_nonoverlapping(pixmap.data().as_ptr(), bits.cast(), pixmap.data().len());
        let result = CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            hbmColor: color,
            hbmMask: mask,
            ..Default::default()
        })
        .map(Icon)
        .map_err(|e| e.to_string());
        let _ = DeleteObject(mask);
        let _ = DeleteObject(color);
        result
    }
}
