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

/// The image a window is matched by. Windows reports a classic console
/// window as owned by its shell (`cmd.exe`, `pwsh.exe`), not by the
/// `conhost.exe` that draws it, so the console class names conhost.
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
fn window_image(class: &str, process_image: String) -> String {
    if class == "ConsoleWindowClass" {
        "conhost.exe".to_owned()
    } else {
        process_image
    }
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

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActivationFailure {
    Cancelled,
    Unavailable,
    Blocked,
}

/// Main-thread activation seam. Each effect follows a foreground sample,
/// after any potentially slow validation of the destination.
#[cfg(any(target_os = "windows", test))]
pub(super) trait Foreground {
    fn revalidate(&mut self, target: Target) -> Option<Target>;
    fn current(&self) -> Option<isize>;
    fn restore_minimized(&mut self, target: Target) -> bool;
    fn raise(&mut self, target: Target) -> bool;
    fn acquire_overlay(
        &mut self,
        previous: Option<isize>,
    ) -> Result<Option<isize>, ActivationFailure>;
    fn release_overlay(&mut self, overlay: isize, previous: Option<isize>);
}

#[cfg(any(target_os = "windows", test))]
pub(super) fn activate(
    api: &mut (impl Foreground + ?Sized),
    target: Target,
    previous: Option<isize>,
) -> Result<Option<isize>, ActivationFailure> {
    use ActivationFailure::{Blocked, Cancelled, Unavailable};

    let checked = api.revalidate(target);
    if api.current() != previous {
        return Err(Cancelled);
    }
    let mut target = checked.ok_or(Unavailable)?;
    if previous == Some(target.hwnd) {
        return Ok(None);
    }
    if target.minimized {
        if !api.restore_minimized(target) {
            return Err(if api.current() == previous {
                Blocked
            } else {
                Cancelled
            });
        }
        let checked = api.revalidate(target);
        let current = api.current();
        // A successful restore may itself have activated the target. Leave
        // final identity and success accounting to foreground confirmation.
        if current == Some(target.hwnd) {
            return checked.map(|_| None).ok_or(Unavailable);
        }
        if current != previous {
            return Err(Cancelled);
        }
        target = checked.ok_or(Unavailable)?;
    }
    if api.raise(target) {
        return Ok(None);
    }
    if api.current() != previous {
        return Err(Cancelled);
    }
    let overlay = api.acquire_overlay(previous)?.ok_or(Blocked)?;
    let checked = api.revalidate(target);
    let current = api.current();
    if current != Some(overlay) && current != previous {
        return Err(Cancelled);
    }
    let result = match checked {
        Some(target) if api.raise(target) => return Ok(Some(overlay)),
        Some(_) => Blocked,
        None => Unavailable,
    };
    let current = api.current();
    if current == Some(overlay) {
        api.release_overlay(overlay, previous);
    } else if current != previous {
        return Err(Cancelled);
    }
    Err(result)
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
        EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetWindow, GetWindowLongW,
        GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, WS_EX_TOOLWINDOW,
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
            let mut class = [0u16; 64];
            let class_len = GetClassNameW(hwnd, &mut class).max(0) as usize;
            let image = super::window_image(&String::from_utf16_lossy(&class[..class_len]), image);
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
        let mut console = candidate("tzgo-7f3a");
        console.image = window_image("ConsoleWindowClass", "cmd.exe".into());
        assert!(
            matches!(find(&[console], "tzgo-7f3a"), Match::One(_)),
            "classic console"
        );
        let mut browser = candidate("tzgo-7f3a · GitHub");
        browser.image = window_image("Chrome_WidgetWin_1", "chrome.exe".into());
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

    const ORIGIN: isize = 10;
    const OVERLAY: isize = 20;
    const OTHER_APP: isize = 30;
    const TARGET: Target = Target {
        hwnd: 1,
        pid: 1,
        minimized: false,
    };

    struct ActivationApi {
        current: Option<isize>,
        accept_direct: bool,
        accept_assisted: bool,
        switch_on_validation: Option<Option<isize>>,
        switch_on_refusal: bool,
        effects: Vec<&'static str>,
    }

    impl Default for ActivationApi {
        fn default() -> Self {
            Self {
                current: Some(ORIGIN),
                accept_direct: false,
                accept_assisted: false,
                switch_on_validation: None,
                switch_on_refusal: false,
                effects: Vec::new(),
            }
        }
    }

    impl Foreground for ActivationApi {
        fn revalidate(&mut self, target: Target) -> Option<Target> {
            if let Some(next) = self.switch_on_validation.take() {
                self.current = next;
            }
            Some(target)
        }
        fn current(&self) -> Option<isize> {
            self.current
        }
        fn restore_minimized(&mut self, _: Target) -> bool {
            self.effects.push("restore");
            true
        }
        fn raise(&mut self, _: Target) -> bool {
            self.effects.push("raise");
            if self.current == Some(OVERLAY) {
                return self.accept_assisted;
            }
            if !self.accept_direct && self.switch_on_refusal {
                self.current = Some(OTHER_APP);
            }
            self.accept_direct
        }
        fn acquire_overlay(
            &mut self,
            previous: Option<isize>,
        ) -> Result<Option<isize>, ActivationFailure> {
            assert_eq!(previous, Some(ORIGIN));
            self.effects.push("overlay");
            self.current = Some(OVERLAY);
            Ok(self.current)
        }
        fn release_overlay(&mut self, overlay: isize, previous: Option<isize>) {
            assert_eq!(self.current, Some(overlay));
            self.effects.push("release");
            self.current = previous;
        }
    }

    #[test]
    fn go_to_uses_foreground_fallback_only_when_needed() {
        let mut direct = ActivationApi {
            accept_direct: true,
            ..Default::default()
        };
        assert_eq!(activate(&mut direct, TARGET, Some(ORIGIN)), Ok(None));
        assert_eq!(direct.effects, ["raise"]);

        let mut fallback = ActivationApi {
            accept_assisted: true,
            ..Default::default()
        };
        assert_eq!(
            activate(
                &mut fallback,
                Target {
                    minimized: true,
                    ..TARGET
                },
                Some(ORIGIN)
            ),
            Ok(Some(OVERLAY))
        );
        assert_eq!(fallback.effects, ["restore", "raise", "overlay", "raise"]);

        let mut blocked = ActivationApi::default();
        assert_eq!(
            activate(&mut blocked, TARGET, Some(ORIGIN)),
            Err(ActivationFailure::Blocked)
        );
        assert_eq!(blocked.effects, ["raise", "overlay", "raise", "release"]);
        assert_eq!(blocked.current, Some(ORIGIN));
    }

    #[test]
    fn go_to_cancels_if_the_foreground_changes_during_destination_validation_or_raise() {
        let mut reading = ActivationApi {
            switch_on_validation: Some(Some(OTHER_APP)),
            ..Default::default()
        };
        assert_eq!(
            activate(
                &mut reading,
                Target {
                    minimized: true,
                    ..TARGET
                },
                Some(ORIGIN)
            ),
            Err(ActivationFailure::Cancelled)
        );
        assert!(reading.effects.is_empty());
        assert_eq!(reading.current, Some(OTHER_APP));

        let mut refused = ActivationApi {
            switch_on_refusal: true,
            ..Default::default()
        };
        assert_eq!(
            activate(&mut refused, TARGET, Some(ORIGIN)),
            Err(ActivationFailure::Cancelled)
        );
        assert_eq!(refused.effects, ["raise"]);
        assert_eq!(refused.current, Some(OTHER_APP));
    }
}
