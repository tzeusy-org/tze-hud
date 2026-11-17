//! System shell: runtime-owned state that agents never see or address.
//!
//! The shell owns the human-override semantics. Viewer dismiss and safe mode
//! work without any agent's cooperation and cannot be vetoed.
//!
//! - `chrome.rs`: tab slots, safe-mode flag, keyboard shortcuts, viewer dismiss.
//! - `safe_mode.rs`: suspend/resume every agent's leases.
//! - `system_card.rs`: the runtime's own toast/status card.
//! - `control_panel/`: the HUD control panel (chords, pause, every setting).
//!
//! The safe-mode overlay and other chrome pixels are drawn by the compositor's
//! windowed frame, never by this module.

pub(crate) mod chrome;
pub(crate) mod control_panel;
pub(crate) mod link;
pub(crate) mod safe_mode;
pub(crate) mod system_card;

pub(crate) use chrome::{
    ChromeShortcut, ChromeState, ChromeTab, DismissTileResult, dismiss_tile, handle_shortcut,
};
