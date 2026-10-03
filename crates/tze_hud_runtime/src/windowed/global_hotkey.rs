//! Windows global hotkey for the human safe-mode override (hud-jm8nq.10).
//!
//! `RegisterHotKey` is the only way a chord reaches the unfocused, click-through
//! overlay. It is registered on a dedicated thread with its own message loop so
//! `WM_HOTKEY` never depends on the winit loop, and the thread blocks in
//! `GetMessageW` (zero idle cost). The OS releases the registration when the
//! process exits.

use crate::operator::status::{HotkeyStatus, report_outcome};
use tokio::sync::mpsc::UnboundedSender;
use tze_hud_scene::config::Hotkey;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

const HOTKEY_ID: i32 = 1;

/// Register `hotkey` globally, signal `tx` on every press, and return the
/// registration outcome (waits briefly for the hotkey thread to report).
///
/// A chord another program already owns is left unregistered and reported as
/// `Failed`; the runtime keeps working without the hotkey.
pub(super) fn spawn_global_hotkey(hotkey: Hotkey, tx: UnboundedSender<()>) -> HotkeyStatus {
    let chord = hotkey.to_string();
    let (status_tx, status_rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("safe-mode-hotkey".into())
        .spawn(move || {
            let modifiers = HOT_KEY_MODIFIERS(hotkey.win32_modifiers()) | MOD_NOREPEAT;
            // SAFETY: a null HWND binds the hotkey to this thread's message
            // queue, which this thread pumps below.
            if let Err(error) =
                unsafe { RegisterHotKey(HWND::default(), HOTKEY_ID, modifiers, hotkey.win32_vk()) }
            {
                report_outcome(
                    &status_tx,
                    HotkeyStatus::Failed {
                        chord: hotkey.to_string(),
                        reason: error.to_string(),
                    },
                );
                return;
            }
            report_outcome(
                &status_tx,
                HotkeyStatus::Registered {
                    chord: hotkey.to_string(),
                },
            );
            let mut msg = MSG::default();
            // SAFETY: `msg` is a valid out-pointer; GetMessageW returns 0 on
            // WM_QUIT and -1 on error, both of which end the loop.
            while unsafe { GetMessageW(&mut msg, HWND::default(), 0, 0) }.0 > 0 {
                if msg.message == WM_HOTKEY && tx.send(()).is_err() {
                    break;
                }
            }
        });
    match spawned {
        Err(error) => HotkeyStatus::Failed {
            chord,
            reason: format!("could not start hotkey thread: {error}"),
        },
        // A slow thread is not a failure: it records its own outcome when it
        // finds the starter gone (see `report_outcome`).
        Ok(_) => status_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or(HotkeyStatus::Pending { chord }),
    }
}
