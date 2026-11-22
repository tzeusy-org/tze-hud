//! The OS lookups the compositor's font loading needs, read here so the
//! renderer does not link the Windows API: the user default locale and the
//! system font table (the `Fonts` registry key under `HKLM`, then `HKCU`).
//! Off Windows nothing is installed and the compositor keeps bundled faces.

/// Install the lookups into the compositor's font loading. Call before the
/// first compositor is built; later calls are no-ops.
pub fn install() {
    #[cfg(windows)]
    tze_hud_compositor::fonts::install_os_font_lookups(tze_hud_compositor::fonts::OsFontLookups {
        user_locale,
        font_entries,
    });
}

#[cfg(windows)]
pub use imp::{font_entries, user_locale};

#[cfg(windows)]
mod imp {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Foundation::ERROR_NO_MORE_ITEMS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ,
        REG_VALUE_TYPE, RegCloseKey, RegEnumValueW, RegOpenKeyExW,
    };
    use windows::core::{PCWSTR, PWSTR, w};

    const FONTS_KEY: PCWSTR = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Fonts");

    /// The Windows user default locale name, e.g. `ja-JP`.
    pub fn user_locale() -> Option<String> {
        use windows::Win32::Globalization::GetUserDefaultLocaleName;
        // LOCALE_NAME_MAX_LENGTH is 85 UTF-16 units including the terminator.
        let mut buf = [0u16; 85];
        // SAFETY: the slice is live and its length is passed by the binding.
        let len = unsafe { GetUserDefaultLocaleName(&mut buf) };
        (len > 1).then(|| String::from_utf16_lossy(&buf[..(len - 1) as usize]))
    }

    /// Machine fonts (relative file names under `%WINDIR%\Fonts`), then
    /// per-user fonts (absolute paths).
    pub fn font_entries() -> Vec<(String, String)> {
        let mut entries = font_values(HKEY_LOCAL_MACHINE);
        entries.extend(font_values(HKEY_CURRENT_USER));
        entries
    }

    /// `(value name, string data)` pairs of the `Fonts` key under `root`.
    fn font_values(root: HKEY) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut key = HKEY::default();
        // SAFETY: FONTS_KEY is a static NUL-terminated wide string; `key`
        // is written by the call and closed below.
        if unsafe { RegOpenKeyExW(root, FONTS_KEY, 0, KEY_READ, &mut key) }.is_err() {
            return out;
        }
        let mut name_buf = vec![0u16; 1024];
        let mut data_buf = vec![0u8; 2048];
        // Bounded: the walk ends at ERROR_NO_MORE_ITEMS long before this.
        for index in 0..100_000 {
            let mut name_len = name_buf.len() as u32;
            let mut data_len = data_buf.len() as u32;
            let mut kind = 0u32;
            // SAFETY: buffers are live and their lengths are passed in
            // elements (name) / bytes (data) as the API requires.
            let rc = unsafe {
                RegEnumValueW(
                    key,
                    index,
                    PWSTR(name_buf.as_mut_ptr()),
                    &mut name_len,
                    None,
                    Some(&mut kind),
                    Some(data_buf.as_mut_ptr()),
                    Some(&mut data_len),
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc.is_err() {
                // e.g. ERROR_MORE_DATA: a value larger than the buffers
                // is not a font entry we can use; skip it.
                continue;
            }
            let kind = REG_VALUE_TYPE(kind);
            if kind != REG_SZ && kind != REG_EXPAND_SZ {
                continue;
            }
            let name = OsString::from_wide(&name_buf[..name_len as usize])
                .to_string_lossy()
                .into_owned();
            let wide: Vec<u16> = data_buf[..data_len as usize]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&u| u != 0)
                .collect();
            let data = OsString::from_wide(&wide).to_string_lossy().into_owned();
            out.push((name, data));
        }
        // SAFETY: `key` was opened above and is closed exactly once.
        unsafe {
            let _ = RegCloseKey(key);
        }
        out
    }
}
