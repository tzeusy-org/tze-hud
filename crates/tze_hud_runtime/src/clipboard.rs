//! The Windows clipboard, as plain text: the composer and the control panel
//! write to it, and the panel's text fields paste from it.

/// CF_UNICODETEXT: UTF-16, NUL-terminated.
#[cfg(target_os = "windows")]
const CF_UNICODETEXT_FORMAT: u32 = 13;

/// Holds the clipboard open (no owner window) until dropped.
#[cfg(target_os = "windows")]
struct OpenClipboardGuard;

#[cfg(target_os = "windows")]
impl OpenClipboardGuard {
    fn open() -> Result<Self, String> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::DataExchange::OpenClipboard;
        // SAFETY: Opening with no owner window is allowed from any thread;
        // the guard balances it with CloseClipboard.
        unsafe { OpenClipboard(HWND::default()) }
            .map(|()| Self)
            .map_err(|_| "the clipboard is busy; try again".to_owned())
    }
}

#[cfg(target_os = "windows")]
impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: Balances the successful OpenClipboard in `open`.
        unsafe {
            let _ = windows::Win32::System::DataExchange::CloseClipboard();
        }
    }
}

/// Read the clipboard's text (CF_UNICODETEXT); empty when it holds none.
///
/// Like [`write_text`], it can block while another process holds the
/// clipboard, so async callers use `spawn_blocking`.
#[cfg(target_os = "windows")]
pub(crate) fn read_text() -> Result<String, String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{GetClipboardData, IsClipboardFormatAvailable};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    let _guard = OpenClipboardGuard::open()?;
    // SAFETY: The clipboard is open for the guard's lifetime. The handle
    // stays owned by the system: we only lock it, read at most its
    // GlobalSize bytes (stopping at the NUL), and unlock it.
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT_FORMAT).is_err() {
            return Ok(String::new());
        }
        let Ok(handle) = GetClipboardData(CF_UNICODETEXT_FORMAT) else {
            return Ok(String::new());
        };
        let hglobal = HGLOBAL(handle.0);
        let units = GlobalSize(hglobal) / std::mem::size_of::<u16>();
        let ptr = GlobalLock(hglobal) as *const u16;
        if ptr.is_null() {
            return Err("couldn't read the clipboard".to_owned());
        }
        let wide = std::slice::from_raw_parts(ptr, units);
        let len = wide.iter().position(|&u| u == 0).unwrap_or(units);
        let text = String::from_utf16_lossy(&wide[..len]);
        let _ = GlobalUnlock(hglobal);
        Ok(text)
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn read_text() -> Result<String, String> {
    Err("there is no clipboard on this platform".to_owned())
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
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

    if text.is_empty() {
        return Ok(());
    }
    let busy = || Err("the clipboard is busy; try again".to_owned());

    // UTF-16, NUL-terminated (CF_UNICODETEXT requires a wide, NUL-terminated buffer).
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);

    let _guard = OpenClipboardGuard::open()?;
    // SAFETY: The clipboard is open (no owner window) until the guard drops
    // at return. The HGLOBAL is allocated with GMEM_MOVEABLE and, on a successful
    // SetClipboardData, ownership transfers to the system (we must NOT free it);
    // on any failure before that transfer the guard/scope frees nothing because
    // GlobalAlloc'd movable memory is reclaimed by the process on exit and the
    // clipboard is closed via the guard. We keep the window narrow.
    unsafe {
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
