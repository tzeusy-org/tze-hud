//! Windows global hotkeys: the pause, control-panel and pairing chords
//! (hud-jm8nq.10, hud-vniri.2).
//!
//! `RegisterHotKey` is the only way a chord reaches the unfocused, click-through
//! overlay. Every chord is registered on one dedicated thread with its own
//! message loop, so `WM_HOTKEY` never depends on the winit loop, and the thread
//! blocks in `GetMessageW` (zero idle cost). The OS releases the registrations
//! when the process exits.

use crate::operator::status::{HotkeyStatus, report_outcome};
use std::time::{Duration, Instant};
use tze_hud_scene::config::{ChordRole, HudChords};
use windows::Win32::Foundation::{ERROR_HOTKEY_ALREADY_REGISTERED, HWND};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, KillTimer, MSG, SetTimer, WM_HOTKEY, WM_TIMER,
};

/// How long to keep retrying a chord that is already registered. A restarted
/// or updated HUD starts while the previous instance still holds the chord, and
/// the handoff gives that instance up to 35 s to exit.
const ALREADY_REGISTERED_RETRY_FOR: Duration = Duration::from_secs(40);
const ALREADY_REGISTERED_RETRY_EVERY_MS: u32 = 250;

/// `RegisterHotKey` id for `role`; `WM_HOTKEY` carries it back in `wParam`.
fn hotkey_id(role: ChordRole) -> i32 {
    match role {
        ChordRole::Pause => 1,
        ChordRole::Panel => 2,
        ChordRole::Pairing => 3,
    }
}

fn role_for_id(id: usize) -> Option<ChordRole> {
    ChordRole::ALL
        .into_iter()
        .find(|role| hotkey_id(*role) as usize == id)
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
) -> Vec<(ChordRole, HotkeyStatus)> {
    let (status_tx, status_rx) = std::sync::mpsc::channel();
    let thread_roles = roles.clone();
    let spawned = std::thread::Builder::new()
        .name("hud-hotkeys".into())
        .spawn(move || {
            let register = |role: ChordRole| {
                let hotkey = chords.get(role);
                let modifiers = HOT_KEY_MODIFIERS(hotkey.win32_modifiers()) | MOD_NOREPEAT;
                // SAFETY: a null HWND binds the hotkey to this thread's message
                // queue, which this thread pumps below.
                unsafe {
                    RegisterHotKey(
                        HWND::default(),
                        hotkey_id(role),
                        modifiers,
                        hotkey.win32_vk(),
                    )
                }
            };
            let retry_until = Instant::now() + ALREADY_REGISTERED_RETRY_FOR;
            // Try every pending role once; keep the ones another instance
            // still holds while the retry window lasts.
            let attempt = |pending: &mut Vec<ChordRole>| {
                pending.retain(|role| {
                    let chord = chords.get(*role).to_string();
                    match register(*role) {
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
            attempt(&mut pending);
            // SAFETY: a null HWND posts WM_TIMER to this thread's queue; the
            // timer is killed below once nothing is pending.
            let mut timer = (!pending.is_empty()).then(|| unsafe {
                SetTimer(HWND::default(), 0, ALREADY_REGISTERED_RETRY_EVERY_MS, None)
            });
            let mut msg = MSG::default();
            // SAFETY: `msg` is a valid out-pointer; GetMessageW returns 0 on
            // WM_QUIT and -1 on error, both of which end the loop.
            while unsafe { GetMessageW(&mut msg, HWND::default(), 0, 0) }.0 > 0 {
                match msg.message {
                    WM_HOTKEY => {
                        if let Some(role) = role_for_id(msg.wParam.0)
                            && !on_press(role)
                        {
                            break;
                        }
                    }
                    WM_TIMER if timer.is_some() => {
                        attempt(&mut pending);
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
        return roles
            .into_iter()
            .map(|role| {
                let status = HotkeyStatus::Failed {
                    chord: chords.get(role).to_string(),
                    reason: format!("could not start hotkey thread: {error}"),
                };
                (role, status)
            })
            .collect();
    }
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
    outcomes
}
