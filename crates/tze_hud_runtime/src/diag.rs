//! Best-effort file diagnostics for freeze investigation (hud-pi5wx).
//!
//! The overlay launches via a scheduled task with **no stdout/stderr capture**
//! (redirecting them breaks `WS_EX_NOREDIRECTIONBITMAP` transparency), so a
//! `tracing` line emitted during a freeze is lost. This module appends
//! diagnostics to a file instead, so a present-stall leaves a durable trail.
//!
//! Introduced during the hud-pi5wx present-stall investigation and kept as
//! permanent observability: a panic or stall on the compositor/render path
//! would otherwise be invisible in the overlay deployment, leaving no trail to
//! diagnose a freeze after the fact.
//!
//! Path resolution: `TZE_HUD_DIAG_LOG` env var if set, else
//! `hud-diag.log` in the log directory (see [`crate::operator::logs::log_dir`]).

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Resolve the diagnostics log path (best-effort).
fn diag_path() -> PathBuf {
    if let Ok(p) = std::env::var("TZE_HUD_DIAG_LOG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let dir = crate::operator::logs::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join("hud-diag.log")
}

/// Append a single timestamped line to the diagnostics log. Best-effort:
/// any I/O error is swallowed (diagnostics must never affect runtime behaviour).
pub fn diag_write(line: &str) {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let thread = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_string();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(diag_path())
    {
        let _ = writeln!(f, "[{ms}ms][{thread}] {line}");
        let _ = f.flush();
    }
}

/// Install a global panic hook that records every panic (any thread, including
/// the compositor thread) to the diagnostics file with a backtrace, then chains
/// to the previously-installed hook so normal behaviour is preserved.
///
/// This is the catch for HB-2: a silent compositor-thread panic that stops
/// `FrameReadySignal` from ever firing again.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown".to_string());
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        let bt = std::backtrace::Backtrace::force_capture();
        diag_write(&format!("PANIC at {location}: {msg}"));
        diag_write(&format!("PANIC backtrace:\n{bt}"));
        previous(info);
    }));
    diag_write("diag: panic hook installed");
}
