//! Windows global hotkeys: the pause, control-panel and pairing chords
//! (hud-jm8nq.10, hud-vniri.2).
//!
//! `RegisterHotKey` is the only way a chord reaches the unfocused, click-through
//! overlay. Every chord is registered on one dedicated thread with its own
//! message loop, so `WM_HOTKEY` never depends on the winit loop, and the thread
//! blocks in `GetMessageW` (zero idle cost). The OS releases the registrations
//! when the process exits.
//!
//! The control panel changes a chord through [`HotkeyThread::rebind`]: the
//! thread registers the new chord under the role's other id first and only
//! then releases the old one, so a chord Windows refuses (another program
//! holds it) leaves the old one working.

use super::hotkey_bindings::Bindings;
use crate::operator::status::{HotkeyStatus, report_outcome, set_hotkey};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tze_hud_scene::config::{ChordRole, Hotkey, HudChords};
use windows::Win32::Foundation::{ERROR_HOTKEY_ALREADY_REGISTERED, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, KillTimer, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SetTimer, WM_APP,
    WM_HOTKEY, WM_TIMER,
};

/// Posted to the hotkey thread when a rebind request is queued.
const WM_REBIND: u32 = WM_APP + 1;
/// How long the panel waits for the hotkey thread to answer a rebind.
const REBIND_TIMEOUT: Duration = Duration::from_secs(2);

/// A chord change for the hotkey thread, with where to send the outcome.
struct Rebind {
    role: ChordRole,
    hotkey: Hotkey,
    /// After this the panel has given up waiting: the thread drops the
    /// request, so the chord never changes behind the panel's back.
    deadline: Instant,
    reply: mpsc::Sender<Result<(), String>>,
}

/// The running hotkey thread, for changing a chord.
pub(super) struct HotkeyThread {
    thread_id: u32,
    requests: std::sync::Mutex<mpsc::Sender<Rebind>>,
}

impl HotkeyThread {
    /// Bind `role` to `hotkey` (blocking, briefly). On error the old chord
    /// still works.
    pub(super) fn rebind(&self, role: ChordRole, hotkey: Hotkey) -> Result<(), String> {
        let (reply, answer) = mpsc::channel();
        let deadline = Instant::now() + REBIND_TIMEOUT;
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(Rebind {
                role,
                hotkey,
                deadline,
                reply,
            })
            .map_err(|_| "The hotkey thread has stopped".to_owned())?;
        // SAFETY: posting a message id with no pointers in it to a thread id
        // the hotkey thread reported for itself.
        unsafe { PostThreadMessageW(self.thread_id, WM_REBIND, WPARAM(0), LPARAM(0)) }
            .map_err(|e| format!("The hotkey thread did not take the request: {e}"))?;
        // A little past the deadline: an answer sent just before it still
        // arrives.
        answer
            .recv_timeout(REBIND_TIMEOUT + Duration::from_millis(500))
            .map_err(|_| "The hotkey thread did not answer".to_owned())?
    }
}

/// How long to keep retrying a chord that is already registered. A restarted
/// or updated HUD starts while the previous instance still holds the chord, and
/// the handoff gives that instance up to 35 s to exit.
const ALREADY_REGISTERED_RETRY_FOR: Duration = Duration::from_secs(40);
const ALREADY_REGISTERED_RETRY_EVERY_MS: u32 = 250;

/// Register `hotkey` on this thread under `id`.
fn register(id: i32, hotkey: Hotkey) -> windows::core::Result<()> {
    let modifiers = HOT_KEY_MODIFIERS(hotkey.win32_modifiers()) | MOD_NOREPEAT;
    // SAFETY: a null HWND binds the hotkey to this thread's message queue,
    // which this thread pumps.
    unsafe { RegisterHotKey(HWND::default(), id, modifiers, hotkey.win32_vk()) }
}

/// Why Windows refused `hotkey`, for the panel row.
fn refusal(hotkey: Hotkey, error: &windows::core::Error) -> String {
    if error.code() == ERROR_HOTKEY_ALREADY_REGISTERED.to_hresult() {
        format!("Another program uses {hotkey}")
    } else {
        format!("Windows refused {hotkey}: {error}")
    }
}

/// Register the `roles` chords of `chords` globally, call `on_press` with the chord's
/// role for every press (it returns false once nobody listens, which ends the
/// thread), and return each role's registration outcome (waits briefly for the
/// hotkey thread to report).
///
/// A chord that is already registered is retried for
/// [`ALREADY_REGISTERED_RETRY_FOR`] on a message-loop timer (status `Pending`
/// meanwhile), since a restart or update starts before the previous instance
/// releases it; chords that did register keep working during the retry. A
/// chord another program keeps is then reported as `Failed`; the runtime keeps
/// working without it.
pub(super) fn spawn_global_hotkeys(
    chords: HudChords,
    roles: Vec<ChordRole>,
    on_press: impl Fn(ChordRole) -> bool + Send + 'static,
) -> (Vec<(ChordRole, HotkeyStatus)>, Option<HotkeyThread>) {
    let (status_tx, status_rx) = mpsc::channel();
    let (thread_tx, thread_rx) = mpsc::channel();
    let (requests_tx, requests) = mpsc::channel::<Rebind>();
    let thread_roles = roles.clone();
    let spawned = std::thread::Builder::new()
        .name("hud-hotkeys".into())
        .spawn(move || {
            let mut bindings = Bindings::new(chords);
            let mut msg = MSG::default();
            // SAFETY: creates this thread's message queue so a rebind posted
            // right after the id is reported is not lost; `msg` is valid.
            let _ = unsafe { PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_NOREMOVE) };
            // SAFETY: no arguments; returns this thread's id.
            let _ = thread_tx.send(unsafe { GetCurrentThreadId() });
            let retry_until = Instant::now() + ALREADY_REGISTERED_RETRY_FOR;
            // Try every pending role once; keep the ones another instance
            // still holds while the retry window lasts.
            let attempt = |pending: &mut Vec<ChordRole>, bindings: &Bindings| {
                pending.retain(|role| {
                    let chord = bindings.chord(*role).to_string();
                    match register(bindings.id(*role), bindings.chord(*role)) {
                        Ok(()) => {
                            report_outcome(&status_tx, *role, HotkeyStatus::Registered { chord });
                            false
                        }
                        Err(error)
                            if error.code() == ERROR_HOTKEY_ALREADY_REGISTERED.to_hresult()
                                && Instant::now() < retry_until =>
                        {
                            true
                        }
                        Err(error) => {
                            report_outcome(
                                &status_tx,
                                *role,
                                HotkeyStatus::Failed {
                                    chord,
                                    reason: error.to_string(),
                                },
                            );
                            false
                        }
                    }
                });
            };
            let mut pending = thread_roles;
            attempt(&mut pending, &bindings);
            // SAFETY: a null HWND posts WM_TIMER to this thread's queue; the
            // timer is killed below once nothing is pending.
            let mut timer = (!pending.is_empty()).then(|| unsafe {
                SetTimer(HWND::default(), 0, ALREADY_REGISTERED_RETRY_EVERY_MS, None)
            });
            // SAFETY: `msg` is a valid out-pointer; GetMessageW returns 0 on
            // WM_QUIT and -1 on error, both of which end the loop.
            while unsafe { GetMessageW(&mut msg, HWND::default(), 0, 0) }.0 > 0 {
                match msg.message {
                    WM_HOTKEY => {
                        if let Some(role) = Bindings::role_for_id(msg.wParam.0)
                            && !on_press(role)
                        {
                            break;
                        }
                    }
                    WM_REBIND => {
                        while let Ok(request) = requests.try_recv() {
                            if Instant::now() > request.deadline {
                                continue;
                            }
                            let outcome = bindings
                                .rebind(request.role, request.hotkey, register, |old| {
                                    // SAFETY: `old` is this thread's id for the
                                    // role; failing means it was never in.
                                    let _ = unsafe { UnregisterHotKey(HWND::default(), old) };
                                })
                                .map(|()| {
                                    // A retry for the old chord is moot now.
                                    pending.retain(|r| *r != request.role);
                                    set_hotkey(
                                        request.role,
                                        HotkeyStatus::Registered {
                                            chord: request.hotkey.to_string(),
                                        },
                                    );
                                })
                                .map_err(|error| refusal(request.hotkey, &error));
                            let _ = request.reply.send(outcome);
                        }
                    }
                    WM_TIMER if timer.is_some() => {
                        attempt(&mut pending, &bindings);
                        if pending.is_empty()
                            && let Some(id) = timer.take()
                        {
                            // SAFETY: `id` is the thread timer created above.
                            let _ = unsafe { KillTimer(HWND::default(), id) };
                        }
                    }
                    _ => {}
                }
            }
        });
    if let Err(error) = spawned {
        let outcomes = roles
            .into_iter()
            .map(|role| {
                let status = HotkeyStatus::Failed {
                    chord: chords.get(role).to_string(),
                    reason: format!("could not start hotkey thread: {error}"),
                };
                (role, status)
            })
            .collect();
        return (outcomes, None);
    }
    let thread = thread_rx
        .recv_timeout(Duration::from_secs(2))
        .ok()
        .map(|thread_id| HotkeyThread {
            thread_id,
            requests: std::sync::Mutex::new(requests_tx),
        });
    // A slow thread is not a failure: it records its own outcome when it finds
    // the starter gone (see `report_outcome`).
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut outcomes: Vec<(ChordRole, HotkeyStatus)> = Vec::new();
    while outcomes.len() < roles.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        match status_rx.recv_timeout(left) {
            Ok(outcome) => outcomes.push(outcome),
            Err(_) => break,
        }
    }
    for role in roles {
        if !outcomes.iter().any(|(r, _)| *r == role) {
            let chord = chords.get(role).to_string();
            outcomes.push((role, HotkeyStatus::Pending { chord }));
        }
    }
    (outcomes, thread)
}
