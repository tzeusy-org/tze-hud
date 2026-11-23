//! One-shot, owner-initiated window matching for notification cards.

#[cfg(any(target_os = "windows", test))]
pub(super) const DEFAULT_APPS: &[&str] = &[
    "WindowsTerminal.exe",
    "Code.exe",
    "Code - Insiders.exe",
    "Cursor.exe",
    "OpenConsole.exe",
    "conhost.exe",
    "wezterm-gui.exe",
    "alacritty.exe",
    "mintty.exe",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub hwnd: isize,
    pub pid: u32,
    pub minimized: bool,
}

#[derive(Debug)]
pub(super) struct Window {
    pub target: Target,
    pub title: String,
    pub image: String,
    pub visible: bool,
    pub owned: bool,
    pub cloaked: bool,
    pub tool: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Match {
    None,
    One(Target),
    Multiple(usize),
}

fn matches_title(title: &str, token: &str) -> bool {
    let title = title.to_lowercase();
    let token = token.to_lowercase();
    let is_token_byte = |c: &u8| c.is_ascii_alphanumeric() || b"._-".contains(c);
    !token.is_empty()
        && title.char_indices().any(|(start, _)| {
            if !title[start..].starts_with(&token) {
                return false;
            }
            let before = start.checked_sub(1).and_then(|i| title.as_bytes().get(i));
            let after = title.as_bytes().get(start + token.len());
            !before.is_some_and(is_token_byte) && !after.is_some_and(is_token_byte)
        })
}

fn eligible(window: &Window, current_pid: u32, apps: &[String], token: &str) -> bool {
    window.visible
        && !window.owned
        && !window.cloaked
        && !window.tool
        && window.target.pid != current_pid
        && apps
            .iter()
            .any(|app| window.image.eq_ignore_ascii_case(app))
        && matches_title(&window.title, token)
}

pub(super) fn find(windows: &[Window], current_pid: u32, apps: &[String], token: &str) -> Match {
    let mut eligible = windows
        .iter()
        .filter(|window| eligible(window, current_pid, apps, token));
    let Some(first) = eligible.next() else {
        return Match::None;
    };
    let rest = eligible.count();
    if rest == 0 {
        Match::One(first.target)
    } else {
        Match::Multiple(1 + rest)
    }
}

/// The full press scan decides uniqueness. Before acting, confirm that its
/// one destination still has the same identity, qualifying title and state.
#[cfg(any(target_os = "windows", test))]
pub(super) fn revalidate(
    target: Target,
    apps: &[String],
    token: &str,
    observe: impl FnOnce(Target) -> Option<Window>,
) -> Option<Target> {
    let window = observe(target)?;
    (window.target.hwnd == target.hwnd
        && window.target.pid == target.pid
        && eligible(&window, std::process::id(), apps, token))
    .then_some(window.target)
}

/// Main-thread activation seam. Failed overlay-assisted attempts put back
/// the foreground that the temporary acquisition displaced.
#[cfg(any(target_os = "windows", test))]
pub(super) trait Foreground {
    fn restore_minimized(&mut self, target: Target) -> bool;
    fn raise(&mut self, target: Target) -> bool;
    fn acquire_overlay(&mut self) -> bool;
    fn release_overlay(&mut self);
}

#[cfg(any(target_os = "windows", test))]
pub(super) fn activate(api: &mut (impl Foreground + ?Sized), target: Target) -> bool {
    if target.minimized && !api.restore_minimized(target) {
        return false;
    }
    if api.raise(target) {
        return true;
    }
    if api.acquire_overlay() && api.raise(target) {
        return true;
    }
    api.release_overlay();
    false
}

#[cfg(target_os = "windows")]
pub(super) mod native {
    use super::{Target, Window};
    use windows::Win32::Foundation::{BOOL, CloseHandle, HWND, LPARAM};
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowTextW,
        GetWindowThreadProcessId, IsIconic, IsWindowVisible, WS_EX_TOOLWINDOW,
    };
    use windows::core::PWSTR;

    pub(in crate::windowed) fn enumerate() -> Vec<Window> {
        unsafe extern "system" fn collect(hwnd: HWND, output: LPARAM) -> BOOL {
            // SAFETY: EnumWindows calls this synchronously with the vector below.
            if let Some(window) = read_window(hwnd) {
                unsafe { &mut *(output.0 as *mut Vec<Window>) }.push(window);
            }
            BOOL(1)
        }
        let mut windows = Vec::new();
        // SAFETY: only the callback borrows the vector during this call.
        if unsafe {
            EnumWindows(
                Some(collect),
                LPARAM(&mut windows as *mut Vec<Window> as isize),
            )
        }
        .is_err()
        {
            windows.clear();
        }
        windows
    }

    fn read_window(hwnd: HWND) -> Option<Window> {
        // SAFETY: the queries only read the HWND and write the local buffers.
        // Exclude our own process before reading the caption: GetWindowTextW
        // reads a foreign caption without sending the app a window message.
        unsafe {
            let mut pid = 0;
            if GetWindowThreadProcessId(hwnd, Some(&mut pid)) == 0 || pid == std::process::id() {
                return None;
            }
            if !IsWindowVisible(hwnd).as_bool()
                || GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.is_invalid())
                || GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0
            {
                return None;
            }
            let mut cloaked = 0u32;
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                (&mut cloaked as *mut u32).cast(),
                size_of::<u32>() as u32,
            )
            .ok()?;
            if cloaked != 0 {
                return None;
            }
            let mut caption = vec![0u16; 32768];
            let len = GetWindowTextW(hwnd, &mut caption) as usize;
            if len == 0 || len >= caption.len() - 1 {
                return None;
            }
            let title = String::from_utf16_lossy(&caption[..len]);
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut size = caption.len() as u32;
            let result = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(caption.as_mut_ptr()),
                &mut size,
            );
            let _ = CloseHandle(handle);
            result.ok()?;
            let path = String::from_utf16_lossy(&caption[..size as usize]);
            let image = path.rsplit(['\\', '/']).next()?.to_owned();
            let target = Target {
                hwnd: hwnd.0 as isize,
                pid,
                minimized: IsIconic(hwnd).as_bool(),
            };
            Some(Window {
                target,
                title,
                image,
                visible: true,
                owned: false,
                cloaked: false,
                tool: false,
            })
        }
    }

    pub(in crate::windowed) fn observe(target: Target) -> Option<Window> {
        read_window(hwnd(target)?)
    }

    pub(in crate::windowed) fn hwnd(target: Target) -> Option<HWND> {
        let hwnd = HWND(target.hwnd as *mut std::ffi::c_void);
        let mut pid = 0;
        // SAFETY: read only. A closed target or a re-used handle is rejected.
        let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        (thread != 0 && pid == target.pid).then_some(hwnd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_to_matches_exactly_one_eligible_whole_session_token() {
        let apps = DEFAULT_APPS
            .iter()
            .map(|app| (*app).to_owned())
            .collect::<Vec<_>>();
        let candidate = |title: &str| Window {
            target: Target {
                hwnd: 10,
                pid: 2,
                minimized: false,
            },
            title: title.into(),
            image: "CODE.EXE".into(),
            visible: true,
            owned: false,
            cloaked: false,
            tool: false,
        };
        let find = |windows: &[Window], token| super::find(windows, 1, &apps, token);
        for (title, token) in [
            ("shell: TZGO-7F3A · Terminal", "tzgo-7f3a"),
            ("— Build Box —", "build box"),
            ("xa a a", "a a"),
            ("tze-é", "TZE-É"),
        ] {
            assert!(
                matches!(find(&[candidate(title)], token), Match::One(_)),
                "{title}"
            );
        }
        for (title, token) in [
            ("tzgo-7f3a-old", "tzgo-7f3a"),
            ("build box.test", "build box"),
            ("pre_build box", "build box"),
            ("abuild box", "build box"),
            ("build box", ""),
        ] {
            assert_eq!(find(&[candidate(title)], token), Match::None, "{title}");
        }
        let mut browser = candidate("tzgo-7f3a · GitHub");
        browser.image = "chrome.exe".into();
        let mut hidden = candidate("tzgo-7f3a");
        hidden.visible = false;
        let mut owned = candidate("tzgo-7f3a");
        owned.owned = true;
        let mut cloaked = candidate("tzgo-7f3a");
        cloaked.cloaked = true;
        let mut tool = candidate("tzgo-7f3a");
        tool.tool = true;
        let mut hud = candidate("tzgo-7f3a");
        hud.target.pid = 1;
        assert_eq!(
            find(&[browser, hidden, owned, cloaked, tool, hud], "tzgo-7f3a"),
            Match::None
        );
        assert_eq!(
            find(
                &[candidate("tzgo-7f3a"), candidate("tzgo-7f3a")],
                "tzgo-7f3a"
            ),
            Match::Multiple(2),
        );
    }

    #[test]
    fn go_to_rejects_a_changed_title_or_eligibility_on_the_same_hwnd() {
        let target = Target {
            hwnd: 4,
            pid: std::process::id() + 1,
            minimized: false,
        };
        let apps = ["Code.exe".to_string()];
        let observe = |title: &str, cloaked| Window {
            target: Target {
                minimized: true,
                ..target
            },
            title: title.into(),
            image: "Code.exe".into(),
            visible: true,
            owned: false,
            cloaked,
            tool: false,
        };
        let token = "build box";
        assert_eq!(
            revalidate(target, &apps, token, |_| Some(observe("renamed", false))),
            None
        );
        assert_eq!(
            revalidate(target, &apps, token, |_| Some(observe(token, true))),
            None
        );
        assert_eq!(
            revalidate(target, &apps, token, |_| Some(observe(token, false))),
            Some(Target {
                minimized: true,
                ..target
            }),
        );
    }

    #[test]
    fn go_to_uses_foreground_fallback_only_when_needed() {
        #[derive(Default)]
        struct Api {
            overlay: bool,
            handoff: bool,
            calls: Vec<&'static str>,
        }
        impl Foreground for Api {
            fn restore_minimized(&mut self, _: Target) -> bool {
                self.calls.push("restore");
                true
            }
            fn raise(&mut self, _: Target) -> bool {
                self.calls.push("raise");
                self.handoff
            }
            fn acquire_overlay(&mut self) -> bool {
                self.calls.push("overlay");
                self.handoff = self.overlay;
                true
            }
            fn release_overlay(&mut self) {
                self.calls.push("release");
            }
        }
        let target = Target {
            hwnd: 1,
            pid: 1,
            minimized: false,
        };
        let mut direct = Api {
            handoff: true,
            ..Default::default()
        };
        assert!(activate(&mut direct, target));
        assert_eq!(direct.calls, ["raise"]);
        let mut fallback = Api {
            overlay: true,
            ..Default::default()
        };
        assert!(activate(
            &mut fallback,
            Target {
                minimized: true,
                ..target
            }
        ));
        assert_eq!(fallback.calls, ["restore", "raise", "overlay", "raise"]);
        let mut blocked = Api::default();
        assert!(!activate(&mut blocked, target));
        assert_eq!(blocked.calls, ["raise", "overlay", "raise", "release"]);
    }
}
