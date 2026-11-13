//! Windows user preferences the HUD follows like any other app (hud-ljhku.13).
//!
//! - Wheel lines per notch (`SPI_GETWHEELSCROLLLINES`), read at each wheel
//!   event: winit surfaces no `WM_SETTINGCHANGE`, and the read is cheap, so a
//!   settings change applies from the next notch.
//! - "Show animations" (`SPI_GETCLIENTAREAANIMATION`), read at startup and on
//!   each config reload; it becomes a design-token layer
//!   ([`tze_hud_config::tokens::apply_os_motion_layer`]). There is no live
//!   OS watch.
//!
//! Other platforms report today's behavior: fixed 40 px notches and
//! animations on.

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
}
