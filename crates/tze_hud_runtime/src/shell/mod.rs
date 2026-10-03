//! System shell: runtime-owned state that agents never see or address.
//!
//! The shell owns the human-override semantics. Viewer dismiss, safe mode, and
//! freeze work without any agent's cooperation and cannot be vetoed.
//!
//! - `chrome.rs`: tab slots, safe-mode flag, keyboard shortcuts, viewer dismiss.
//! - `safe_mode.rs`: suspend/resume every agent's leases.
//! - `freeze.rs`: freeze queue semantics.
//! - `system_card.rs`: the runtime's own toast/status card.
//!
//! The safe-mode overlay and other chrome pixels are drawn by the compositor's
//! windowed frame, never by this module.

pub(crate) mod chrome;
// Freeze is unwired in production; hud-bstmy.5.6 decides delete-or-wire. Drop
// this expectation (and the dead items) with that decision.
#[expect(dead_code, reason = "freeze is never activated; see hud-bstmy.5.6")]
pub(crate) mod freeze;
pub(crate) mod safe_mode;
pub(crate) mod system_card;

pub(crate) use chrome::{
    ChromeShortcut, ChromeState, ChromeTab, DismissTileResult, dismiss_tile, handle_shortcut,
};
