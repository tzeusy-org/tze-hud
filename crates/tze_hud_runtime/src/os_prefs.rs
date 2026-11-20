//! Windows user preferences the HUD follows like any other app (hud-ljhku.13).
//!
//! - Wheel lines per notch (`SPI_GETWHEELSCROLLLINES`), read at each wheel
//!   event: winit surfaces no `WM_SETTINGCHANGE`, and the read is cheap, so a
//!   settings change applies from the next notch.
//! - "Show animations" (`SPI_GETCLIENTAREAANIMATION`), read at startup and on
//!   each config reload; it becomes a design-token layer
//!   ([`tze_hud_config::tokens::apply_os_motion_layer`]). There is no live
//!   OS watch.
//! - Text size (Settings › Accessibility › Text size, HKCU
//!   `Software\Microsoft\Accessibility` `TextScaleFactor`) and the caret
//!   blink time (`GetCaretBlinkTime`), read with the animation preference
//!   (hud-ohgc9.12): text size multiplies the owner's `ui.scale`, and a caret
//!   with blinking off is steady
//!   ([`tze_hud_config::tokens::apply_os_text_and_caret`]). Nothing in the
//!   config overrides either.
//!
//! Other platforms report today's behavior: fixed 40 px notches, animations
//! on, text size 100% and a 530 ms caret blink.

use tze_hud_config::tokens::{
    CARET_BLINK_MS_RANGE, DEFAULT_CARET_BLINK_MS, OsPreferences, TEXT_SCALE_RANGE,
};

/// Windows "lines per notch" wheel setting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum WheelLines {
    /// Not Windows: a notch is a fixed number of pixels
    /// ([`tze_hud_input::WHEEL_LINE_PX`]). Only built off Windows.
    #[cfg_attr(windows, allow(dead_code))]
    Fixed,
    /// This many text lines per notch (0 disables wheel scrolling).
    Lines(u32),
    /// `WHEEL_PAGESCROLL`: one page per notch.
    Page,
}

/// Windows' default when the setting cannot be read.
#[cfg_attr(not(windows), allow(dead_code))]
const DEFAULT_WHEEL_LINES: u32 = 3;

/// `WHEEL_PAGESCROLL` (`UINT_MAX`).
#[cfg_attr(not(windows), allow(dead_code))]
const WHEEL_PAGESCROLL: u32 = u32::MAX;

/// Map a raw `SPI_GETWHEELSCROLLLINES` value (`None` when the call failed).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn wheel_lines_from_raw(raw: Option<u32>) -> WheelLines {
    match raw {
        Some(WHEEL_PAGESCROLL) => WheelLines::Page,
        Some(n) => WheelLines::Lines(n),
        None => WheelLines::Lines(DEFAULT_WHEEL_LINES),
    }
}

/// The current wheel setting; on Windows, read from the OS on every call.
pub(crate) fn wheel_lines() -> WheelLines {
    #[cfg(windows)]
    {
        wheel_lines_from_raw(spi_get_u32(
            windows::Win32::UI::WindowsAndMessaging::SPI_GETWHEELSCROLLLINES,
        ))
    }
    #[cfg(not(windows))]
    {
        WheelLines::Fixed
    }
}

/// Whether the OS allows client-area animation ("Show animations"). True
/// when unreadable and off Windows.
pub(crate) fn animations_enabled() -> bool {
    #[cfg(windows)]
    {
        spi_get_u32(windows::Win32::UI::WindowsAndMessaging::SPI_GETCLIENTAREAANIMATION)
            .is_none_or(|v| v != 0)
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Map a raw `TextScaleFactor` percentage (`None` when unreadable) to a
/// factor in [`TEXT_SCALE_RANGE`]; 1 when unreadable.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn text_scale_from_raw(raw: Option<u32>) -> f32 {
    let (lo, hi) = TEXT_SCALE_RANGE;
    raw.map_or(1.0, |pct| (pct as f32 / 100.0).clamp(lo, hi))
}

/// `GetCaretBlinkTime`'s value when caret blinking is off.
#[cfg_attr(not(windows), allow(dead_code))]
const CARET_BLINK_INFINITE: u32 = u32::MAX;

/// Map a raw `GetCaretBlinkTime` value to the caret key's ms: `INFINITE`
/// (blinking off) is `0`, a steady caret; `0` is the call failing and, like
/// an unreadable value, keeps [`DEFAULT_CARET_BLINK_MS`], so a failed read
/// never looks like "off". Others clamp to [`CARET_BLINK_MS_RANGE`].
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn caret_blink_ms_from_raw(raw: Option<u32>) -> u32 {
    let (lo, hi) = CARET_BLINK_MS_RANGE;
    match raw {
        Some(CARET_BLINK_INFINITE) => 0,
        Some(0) | None => DEFAULT_CARET_BLINK_MS,
        Some(ms) => ms.clamp(lo, hi),
    }
}

/// The Windows text size as a factor; 1 off Windows.
fn text_scale() -> f32 {
    #[cfg(windows)]
    {
        text_scale_from_raw(registry_dword(
            windows::core::w!("Software\\Microsoft\\Accessibility"),
            windows::core::w!("TextScaleFactor"),
        ))
    }
    #[cfg(not(windows))]
    {
        1.0
    }
}

/// The Windows caret blink half-period in ms (`0` = steady); 530 off Windows.
fn caret_blink_ms() -> u32 {
    #[cfg(windows)]
    {
        // SAFETY: no arguments; reads a user setting.
        caret_blink_ms_from_raw(Some(unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetCaretBlinkTime()
        }))
    }
    #[cfg(not(windows))]
    {
        DEFAULT_CARET_BLINK_MS
    }
}

/// Every Windows preference the HUD takes as a token layer, read now: the
/// runtime calls this at startup and on each config reload. Harnesses and
/// tests pin [`OsPreferences::default`] instead.
pub(crate) fn read() -> OsPreferences {
    OsPreferences {
        animations_enabled: animations_enabled(),
        text_scale: text_scale(),
        caret_blink_ms: caret_blink_ms(),
    }
}

/// Read a `REG_DWORD` under `HKEY_CURRENT_USER`; `None` when missing or
/// unreadable.
#[cfg(windows)]
fn registry_dword(subkey: windows::core::PCWSTR, value: windows::core::PCWSTR) -> Option<u32> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: RRF_RT_REG_DWORD restricts the read to 4 bytes, `data` is a
    // live u32 and `size` holds its size.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey,
            value,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut std::ffi::c_void),
            Some(&mut size),
        )
        .is_ok()
        .then_some(data)
    }
}

/// Read a 4-byte `SystemParametersInfoW` value (`UINT` or `BOOL`).
#[cfg(windows)]
fn spi_get_u32(
    action: windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_ACTION,
) -> Option<u32> {
    use std::ffi::c_void;
    use windows::Win32::UI::WindowsAndMessaging::{
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };
    let mut value = 0u32;
    // SAFETY: both actions write one UINT/BOOL (4 bytes) through pvParam,
    // which points at a live u32; reading has no side effect.
    unsafe {
        SystemParametersInfoW(
            action,
            0,
            Some(&mut value as *mut u32 as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
        .then_some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_wheel_setting_maps_lines_page_and_default() {
        assert_eq!(wheel_lines_from_raw(Some(5)), WheelLines::Lines(5));
        assert_eq!(wheel_lines_from_raw(Some(0)), WheelLines::Lines(0));
        assert_eq!(wheel_lines_from_raw(Some(u32::MAX)), WheelLines::Page);
        assert_eq!(wheel_lines_from_raw(None), WheelLines::Lines(3));
    }

    #[test]
    fn non_windows_keeps_fixed_notches_and_animation() {
        if cfg!(windows) {
            return;
        }
        assert_eq!(wheel_lines(), WheelLines::Fixed);
        assert!(animations_enabled());
    }

    /// hud-ohgc9.12: 100–225%, other values clamp, unreadable is 100%.
    #[test]
    fn raw_text_size_maps_to_a_clamped_factor() {
        assert_eq!(text_scale_from_raw(Some(100)), 1.0);
        assert_eq!(text_scale_from_raw(Some(125)), 1.25);
        assert_eq!(text_scale_from_raw(Some(225)), 2.25);
        assert_eq!(text_scale_from_raw(Some(300)), 2.25);
        assert_eq!(text_scale_from_raw(Some(50)), 1.0);
        assert_eq!(text_scale_from_raw(Some(0)), 1.0);
        assert_eq!(text_scale_from_raw(None), 1.0);
    }

    /// hud-ohgc9.12: INFINITE is steady (0); a failed read (raw 0 or none)
    /// keeps 530 so it never looks like "off"; others clamp to 200–1200.
    #[test]
    fn raw_caret_blink_maps_off_failure_and_clamps() {
        assert_eq!(caret_blink_ms_from_raw(Some(u32::MAX)), 0);
        assert_eq!(caret_blink_ms_from_raw(Some(0)), 530);
        assert_eq!(caret_blink_ms_from_raw(None), 530);
        assert_eq!(caret_blink_ms_from_raw(Some(250)), 250);
        assert_eq!(caret_blink_ms_from_raw(Some(100)), 200);
        assert_eq!(caret_blink_ms_from_raw(Some(5000)), 1200);
    }

    /// R12.3 S1: off Windows every preference is the HUD's default.
    #[test]
    fn non_windows_reads_the_defaults() {
        if cfg!(windows) {
            return;
        }
        assert_eq!(read(), OsPreferences::default());
    }
}
