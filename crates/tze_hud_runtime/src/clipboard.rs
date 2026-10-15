//! The Windows clipboard (write side), shared by the composer and the
//! control panel's Connect card.

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

    if text.is_empty() {
        return Ok(());
    }
    let busy = || Err("the clipboard is busy; try again".to_owned());

    // UTF-16, NUL-terminated (CF_UNICODETEXT requires a wide, NUL-terminated buffer).
    let mut wide: Vec<u16> = text.encode_utf16().collect();
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
pub(crate) fn write_text(_text: &str) -> Result<(), String> {
    Err("there is no clipboard on this platform".to_owned())
}
