//! Local clipboard access shared by the composer, portal OUTPUT and panel.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) type Write = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
type Read = Arc<dyn Fn() -> Result<Option<String>, String> + Send + Sync>;

/// Injectable local I/O; editor state and clipboard contents never leave the runtime.
#[derive(Clone)]
pub(crate) struct Clipboard {
    pub read: Read,
    pub write: Write,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self {
            read: Arc::new(read_text),
            write: Arc::new(write_text),
        }
    }
}

/// Accepted (non-empty) [`write_text`] calls on every platform, for the
/// admin `clipboard_writes` counter (hud-ljhku.2). Never the text itself.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// Monotonic count of accepted clipboard writes since start.
pub(crate) fn writes() -> u64 {
    WRITES.load(Ordering::Relaxed)
}

#[cfg(test)]
thread_local! {
    /// The last text this thread wrote, for tests on the non-Windows stub.
    pub(crate) static LAST_TEXT: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

fn record(text: &str) {
    if !text.is_empty() {
        WRITES.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        LAST_TEXT.with(|last| *last.borrow_mut() = Some(text.to_owned()));
    }
}

/// `text` with every lone `\n` as `\r\n` (existing CRLF pairs unchanged).
#[cfg(any(target_os = "windows", test))]
fn crlf(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\n') {
        return text.into();
    }
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let mut prev = '\0';
    for c in text.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    out.into()
}

/// Write `text` to the Windows clipboard as CF_UNICODETEXT. No-op on empty
/// input.
///
/// Opens the clipboard with no owner window, so any thread may call it; the
/// call is short but can block while another process holds the clipboard, so
/// async callers use `spawn_blocking`. Fails when another process holds the
/// clipboard; callers decide whether to say so (the composer's cut keeps its
/// local edit regardless).
#[cfg(target_os = "windows")]
pub(crate) fn write_text(text: &str) -> Result<(), String> {
    use windows::Win32::Foundation::{HANDLE, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

    const CF_UNICODETEXT_FORMAT: u32 = 13;
    record(text);

    if text.is_empty() {
        return Ok(());
    }
    let busy = || Err("the clipboard is busy; try again".to_owned());

    // UTF-16, NUL-terminated (CF_UNICODETEXT requires a wide, NUL-terminated
    // buffer), with CRLF line breaks as Windows text consumers expect.
    let mut wide: Vec<u16> = crlf(text).encode_utf16().collect();
    wide.push(0);

    // SAFETY: The clipboard is opened with no owner window and closed by the
    // guard before returning. The HGLOBAL is allocated with GMEM_MOVEABLE and, on a successful
    // SetClipboardData, ownership transfers to the system (we must NOT free it);
    // on any failure before that transfer the guard/scope frees nothing because
    // GlobalAlloc'd movable memory is reclaimed by the process on exit and the
    // clipboard is closed via the guard. We keep the window narrow.
    unsafe {
        if OpenClipboard(HWND::default()).is_err() {
            return busy();
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
            return busy();
        }

        let bytes = std::mem::size_of_val(wide.as_slice());
        let Ok(hglobal) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
            return busy();
        };
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return busy();
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
        let _ = GlobalUnlock(hglobal);

        // On success the system takes ownership of `hglobal`; on failure it stays
        // process-owned (reclaimed at exit) — we do not double-free.
        if SetClipboardData(CF_UNICODETEXT_FORMAT, HANDLE(hglobal.0)).is_err() {
            // Ownership was NOT transferred; leave it to the process.
            return busy();
        }
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn write_text(text: &str) -> Result<(), String> {
    record(text);
    Err("there is no clipboard on this platform".to_owned())
}

/// Read CF_UNICODETEXT. No text format is an empty clipboard; access failures
/// remain visible to the panel. Composer callers keep their existing no-op fallback.
#[cfg(target_os = "windows")]
pub(crate) fn read_text() -> Result<Option<String>, String> {
    use windows::Win32::Foundation::{HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    const CF_UNICODETEXT_FORMAT: u32 = 13;
    let busy = || "The clipboard is busy; try again".to_owned();
    // SAFETY: guards balance clipboard and global-memory access; the UTF-16
    // slice is bounded by the allocated object and one editor-cap overflow.
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT_FORMAT).is_err() {
            return Ok(None);
        }
        OpenClipboard(HWND::default()).map_err(|_| busy())?;
        struct ClipboardGuard;
        impl Drop for ClipboardGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseClipboard();
                }
            }
        }
        let _clipboard = ClipboardGuard;
        let handle = GetClipboardData(CF_UNICODETEXT_FORMAT).map_err(|_| busy())?;
        let global = HGLOBAL(handle.0);
        let units = (GlobalSize(global) / std::mem::size_of::<u16>())
            .min(tze_hud_input::composer_draft::MAX_DRAFT_BYTES + 1);
        let ptr = GlobalLock(global);
        if ptr.is_null() {
            return Err(busy());
        }
        let wide = std::slice::from_raw_parts(ptr.cast::<u16>(), units);
        let mut len = wide.iter().position(|c| *c == 0).unwrap_or(units);
        if len != 0 && (0xD800..=0xDBFF).contains(&wide[len - 1]) {
            len -= 1;
        }
        let text = String::from_utf16_lossy(&wide[..len]);
        let _ = GlobalUnlock(global);
        Ok((!text.is_empty()).then_some(text))
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn read_text() -> Result<Option<String>, String> {
    Err("There is no clipboard on this platform".to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn windows_clipboard_text_uses_crlf_line_breaks() {
        assert_eq!(super::crlf("one line"), "one line");
        assert_eq!(super::crlf("a\nb\r\nc\n"), "a\r\nb\r\nc\r\n");
    }
}
