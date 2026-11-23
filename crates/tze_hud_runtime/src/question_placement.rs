//! Where a new question card appears (`[runtime] question_placement`).
//!
//! The scene asks once, when a question is presented (immediately, or when a
//! `delay_ms` question comes due). The answer is the overlaid display of the
//! focused window, of the pointer, or `None` for the notification display.
//! There is no hook, timer or idle work: each placement makes a few OS reads.

use std::sync::{Arc, Mutex};

use tze_hud_compositor::{DisplayLayout, normalize_display_name};
use tze_hud_scene::{PublicationDefaults, QuestionPlacement, QuestionPlacer};

/// The question card zone, which places per display.
const ZONE: &str = "notification-area";

/// `SHQueryUserNotificationState`'s answer (`QUNS_*`, 1-7).
/// Only the Windows probe constructs most of them.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotificationState {
    NotPresent,
    Busy,
    RunningD3dFullScreen,
    PresentationMode,
    AcceptsNotifications,
    QuietTime,
    App,
}

impl NotificationState {
    fn label(self) -> &'static str {
        match self {
            Self::NotPresent => "not_present",
            Self::Busy => "busy",
            Self::RunningD3dFullScreen => "d3d_full_screen",
            Self::PresentationMode => "presentation_mode",
            Self::AcceptsNotifications => "accepts_notifications",
            Self::QuietTime => "quiet_time",
            Self::App => "app",
        }
    }
}

/// The foreground window, when it is an ordinary window of another process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Foreground {
    /// Its monitor's normalized OS name.
    pub display: String,
    /// The OS handle, for the window-rect read.
    pub window: isize,
}

/// Give `scene` its question placer, starting from the saved
/// `[runtime] question_placement` (default `focused`).
pub(crate) fn install(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    config_toml: Option<&str>,
    defaults: &PublicationDefaults,
    layout: Arc<Mutex<DisplayLayout>>,
    probe: Arc<dyn DisplayProbe>,
) {
    let saved = config_toml
        .and_then(tze_hud_config::runtime_section)
        .and_then(|runtime| runtime.question_placement)
        .and_then(|value| QuestionPlacement::parse(&value))
        .unwrap_or_default();
    defaults.set_question_placement(saved);
    scene.overlay.question_placer = Some(Arc::new(DisplayPlacer::new(
        defaults.clone(),
        probe,
        layout,
    )));
}

/// The OS reads one placement takes. Windows reads Win32; tests inject.
pub(crate) trait DisplayProbe: Send + Sync + std::fmt::Debug {
    /// `None` without a foreground window, or when it is the HUD itself or
    /// the shell desktop (`Progman`/`WorkerW`).
    fn foreground(&self) -> Option<Foreground>;
    /// The pointer's (nearest) monitor.
    fn pointer_display(&self) -> Option<String>;
    /// `None` when the query fails.
    fn notification_state(&self) -> Option<NotificationState>;
    /// Whether `window`'s rect is exactly its monitor's rect.
    fn fills_monitor(&self, window: &Foreground) -> bool;
}

/// The scene's [`QuestionPlacer`]: the live setting, the OS reads, and the
/// overlaid displays (kept current by the display publisher).
#[derive(Debug)]
pub(crate) struct DisplayPlacer {
    defaults: PublicationDefaults,
    probe: Arc<dyn DisplayProbe>,
    layout: Arc<Mutex<DisplayLayout>>,
}

impl DisplayPlacer {
    pub(crate) fn new(
        defaults: PublicationDefaults,
        probe: Arc<dyn DisplayProbe>,
        layout: Arc<Mutex<DisplayLayout>>,
    ) -> Self {
        Self {
            defaults,
            probe,
            layout,
        }
    }
}

impl QuestionPlacer for DisplayPlacer {
    fn place(&self) -> Option<String> {
        let placement = self.defaults.question_placement();
        if placement == QuestionPlacement::Fixed {
            return None;
        }
        let layout = self
            .layout
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        place(placement, self.probe.as_ref(), &layout)
    }
}

/// The overlaid display a question goes to; `None` is the notification
/// display (also for a display with no overlay).
pub(crate) fn place(
    placement: QuestionPlacement,
    probe: &dyn DisplayProbe,
    layout: &DisplayLayout,
) -> Option<String> {
    let overlaid = |name: Option<String>| {
        let name = normalize_display_name(&name?);
        layout.display_rect(&name).map(|_| name)
    };
    let notification = notification_display(layout);
    let chosen = match placement {
        QuestionPlacement::Fixed => None,
        QuestionPlacement::Pointer => overlaid(probe.pointer_display()),
        QuestionPlacement::Focused => match probe.foreground() {
            Some(window) => overlaid(Some(window.display.clone()))
                .map(|display| uncovered(probe, layout, &window, display, &notification)),
            None => overlaid(probe.pointer_display()),
        },
    };
    chosen.filter(|display| Some(display) != notification.as_ref())
}

/// The notification area's display: its configured one when connected, else
/// the primary.
fn notification_display(layout: &DisplayLayout) -> Option<String> {
    let displays = layout.displays();
    layout
        .zone_display_rect(ZONE)
        .and_then(|rect| displays.iter().find(|d| d.rect == rect && !d.primary))
        .or_else(|| displays.iter().find(|d| d.primary))
        .map(|d| d.name.clone())
}

/// A display the foreground window chose, moved off when a full-screen game
/// or a presentation covers it.
fn uncovered(
    probe: &dyn DisplayProbe,
    layout: &DisplayLayout,
    window: &Foreground,
    chosen: String,
    notification: &Option<String>,
) -> String {
    let Some(state) = probe.notification_state() else {
        tracing::debug!(
            display = %chosen,
            "question placement: notification state unavailable"
        );
        return chosen;
    };
    let covering = matches!(
        state,
        NotificationState::RunningD3dFullScreen | NotificationState::PresentationMode
    );
    if !covering || !probe.fills_monitor(window) {
        tracing::debug!(
            display = %chosen,
            state = state.label(),
            "question placement: display not covered"
        );
        return chosen;
    }
    let other = |name: &Option<String>| name.as_ref().filter(|n| **n != chosen).cloned();
    let pointer = probe
        .pointer_display()
        .map(|n| normalize_display_name(&n))
        .filter(|n| layout.display_rect(n).is_some());
    let moved = other(&pointer).or_else(|| other(notification)).or_else(|| {
        (state == NotificationState::RunningD3dFullScreen)
            .then(|| {
                let mut displays: Vec<_> = layout.displays().iter().collect();
                displays.sort_by_key(|d| !d.primary);
                displays
                    .into_iter()
                    .map(|d| d.name.clone())
                    .find(|n| *n != chosen)
            })
            .flatten()
    });
    match moved {
        Some(moved) => {
            tracing::info!(
                "question placement: {chosen} covered ({}); placed on {moved}",
                state.label()
            );
            moved
        }
        None => {
            tracing::info!(
                "question placement: {chosen} covered ({}); no other display",
                state.label()
            );
            chosen
        }
    }
}

/// The Win32 reads.
#[cfg(target_os = "windows")]
#[derive(Debug, Default)]
pub(crate) struct Win32Probe;

#[cfg(target_os = "windows")]
mod win32 {
    use super::{Foreground, NotificationState};
    use windows::Win32::Foundation::{HWND, POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITORINFO,
        MONITORINFOEXW, MonitorFromPoint, MonitorFromWindow,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::Shell::{
        QUNS_ACCEPTS_NOTIFICATIONS, QUNS_APP, QUNS_BUSY, QUNS_NOT_PRESENT, QUNS_PRESENTATION_MODE,
        QUNS_QUIET_TIME, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetCursorPos, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId,
    };

    fn monitor_info(monitor: HMONITOR) -> Option<MONITORINFOEXW> {
        if monitor.is_invalid() {
            return None;
        }
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: `info` is a MONITORINFOEXW whose cbSize says so.
        unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }
            .as_bool()
            .then_some(info)
    }

    fn monitor_name(monitor: HMONITOR) -> Option<String> {
        let info = monitor_info(monitor)?;
        let len = info
            .szDevice
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(info.szDevice.len());
        Some(String::from_utf16_lossy(&info.szDevice[..len]))
    }

    fn is_shell_desktop(window: HWND) -> bool {
        let mut class = [0u16; 16];
        // SAFETY: the buffer outlives the call and its length bounds the write.
        let len = unsafe { GetClassNameW(window, &mut class) };
        let class = String::from_utf16_lossy(&class[..len.max(0) as usize]);
        class == "Progman" || class == "WorkerW"
    }

    impl super::DisplayProbe for super::Win32Probe {
        fn foreground(&self) -> Option<Foreground> {
            // SAFETY: these take values or out-params owned here and only read.
            unsafe {
                let window = GetForegroundWindow();
                if window.is_invalid() || is_shell_desktop(window) {
                    return None;
                }
                let mut pid = 0;
                GetWindowThreadProcessId(window, Some(&mut pid));
                if pid == GetCurrentProcessId() {
                    return None;
                }
                let display = monitor_name(MonitorFromWindow(window, MONITOR_DEFAULTTONULL))?;
                Some(Foreground {
                    display,
                    window: window.0 as isize,
                })
            }
        }

        fn pointer_display(&self) -> Option<String> {
            let mut point = POINT::default();
            // SAFETY: GetCursorPos writes the provided POINT.
            unsafe { GetCursorPos(&mut point) }.ok()?;
            // SAFETY: MonitorFromPoint takes values only.
            monitor_name(unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) })
        }

        fn notification_state(&self) -> Option<NotificationState> {
            // SAFETY: no arguments; it returns the state or an error.
            let state = unsafe { SHQueryUserNotificationState() }.ok()?;
            Some(match state {
                QUNS_NOT_PRESENT => NotificationState::NotPresent,
                QUNS_BUSY => NotificationState::Busy,
                QUNS_RUNNING_D3D_FULL_SCREEN => NotificationState::RunningD3dFullScreen,
                QUNS_PRESENTATION_MODE => NotificationState::PresentationMode,
                QUNS_ACCEPTS_NOTIFICATIONS => NotificationState::AcceptsNotifications,
                QUNS_QUIET_TIME => NotificationState::QuietTime,
                QUNS_APP => NotificationState::App,
                _ => return None,
            })
        }

        fn fills_monitor(&self, window: &Foreground) -> bool {
            let hwnd = HWND(window.window as *mut _);
            let mut rect = RECT::default();
            // SAFETY: the out-param is owned here; a stale handle fails the call.
            if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
                return false;
            }
            // SAFETY: MonitorFromWindow takes values only.
            let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) };
            monitor_info(monitor).is_some_and(|info| info.monitorInfo.rcMonitor == rect)
        }
    }
}

/// Off Windows nothing is focused and there is no pointer: every question
/// goes to the notification display.
#[cfg(not(target_os = "windows"))]
#[derive(Debug, Default)]
pub(crate) struct Win32Probe;

#[cfg(not(target_os = "windows"))]
impl DisplayProbe for Win32Probe {
    fn foreground(&self) -> Option<Foreground> {
        None
    }
    fn pointer_display(&self) -> Option<String> {
        None
    }
    fn notification_state(&self) -> Option<NotificationState> {
        None
    }
    fn fills_monitor(&self, _: &Foreground) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tze_hud_compositor::DisplayRect;
    use tze_hud_scene::Rect;

    /// Scripted reads that count each call.
    #[derive(Debug, Default)]
    struct Probe {
        foreground: Option<&'static str>,
        pointer: Option<&'static str>,
        state: Option<NotificationState>,
        fills: bool,
        reads: [AtomicUsize; 4],
    }

    impl Probe {
        fn reads(&self) -> [usize; 4] {
            self.reads.each_ref().map(|r| r.load(Ordering::SeqCst))
        }
    }

    impl DisplayProbe for Probe {
        fn foreground(&self) -> Option<Foreground> {
            self.reads[0].fetch_add(1, Ordering::SeqCst);
            self.foreground.map(|display| Foreground {
                display: display.into(),
                window: 7,
            })
        }
        fn pointer_display(&self) -> Option<String> {
            self.reads[1].fetch_add(1, Ordering::SeqCst);
            self.pointer.map(Into::into)
        }
        fn notification_state(&self) -> Option<NotificationState> {
            self.reads[2].fetch_add(1, Ordering::SeqCst);
            self.state
        }
        fn fills_monitor(&self, _: &Foreground) -> bool {
            self.reads[3].fetch_add(1, Ordering::SeqCst);
            self.fills
        }
    }

    /// DISPLAY1 primary, DISPLAY2 and DISPLAY3 overlaid; the notification
    /// area optionally assigned elsewhere.
    fn layout(notification_on: Option<&str>) -> DisplayLayout {
        let display = |name: &str, x: f32, primary| DisplayRect {
            name: name.into(),
            rect: Rect::new(x, 0.0, 1920.0, 1080.0),
            primary,
        };
        let zones = notification_on
            .map(|d| [(ZONE.to_owned(), d.to_owned())].into())
            .unwrap_or_default();
        DisplayLayout::new(
            vec![
                display(r"\\.\DISPLAY1", 0.0, true),
                display("DISPLAY2", 1920.0, false),
                display("DISPLAY3", 3840.0, false),
            ],
            &zones,
        )
    }

    fn placed(placement: QuestionPlacement, probe: &Probe) -> Option<String> {
        place(placement, probe, &layout(None))
    }

    /// R12.1 S1-S3, plus a display with no overlay.
    #[test]
    fn focused_pointer_and_fixed_choose_their_display() {
        use QuestionPlacement::*;
        let probe = |foreground, pointer| Probe {
            foreground,
            pointer,
            state: Some(NotificationState::AcceptsNotifications),
            ..Default::default()
        };
        // S1: the focused window's monitor.
        assert_eq!(
            placed(Focused, &probe(Some("DISPLAY2"), Some("DISPLAY3"))),
            Some("DISPLAY2".into())
        );
        // S2: no ordinary foreground window -> the pointer's monitor.
        assert_eq!(
            placed(Focused, &probe(None, Some(r"\\.\DISPLAY3"))),
            Some("DISPLAY3".into())
        );
        // S3: pointer whatever is focused; fixed reads nothing.
        assert_eq!(
            placed(Pointer, &probe(Some("DISPLAY2"), Some("DISPLAY3"))),
            Some("DISPLAY3".into())
        );
        let fixed = probe(Some("DISPLAY2"), Some("DISPLAY3"));
        assert_eq!(placed(Fixed, &fixed), None);
        assert_eq!(fixed.reads(), [0, 0, 0, 0]);
        // The notification display, and one without an overlay, are `None`.
        assert_eq!(placed(Focused, &probe(Some("DISPLAY1"), None)), None);
        assert_eq!(placed(Focused, &probe(Some("DISPLAY9"), None)), None);
        assert_eq!(placed(Pointer, &probe(None, None)), None);
        // Fixed with [displays] moving the notification area elsewhere is
        // still `None`: the card draws with the zone.
        assert_eq!(
            place(
                Focused,
                &probe(Some("DISPLAY2"), None),
                &layout(Some("DISPLAY2"))
            ),
            None
        );
        assert_eq!(
            place(
                Focused,
                &probe(Some("DISPLAY1"), None),
                &layout(Some("DISPLAY2"))
            ),
            Some("DISPLAY1".into())
        );
    }

    /// Exactly one sample per placement: three reads for an ordinary
    /// focused window, the window rect only for a covering state.
    #[test]
    fn a_focused_placement_reads_only_what_it_needs() {
        let ordinary = Probe {
            foreground: Some("DISPLAY2"),
            state: Some(NotificationState::AcceptsNotifications),
            ..Default::default()
        };
        placed(QuestionPlacement::Focused, &ordinary);
        assert_eq!(ordinary.reads(), [1, 0, 1, 0]);
        let covering = Probe {
            foreground: Some("DISPLAY2"),
            state: Some(NotificationState::PresentationMode),
            ..Default::default()
        };
        placed(QuestionPlacement::Focused, &covering);
        assert_eq!(covering.reads(), [1, 0, 1, 1]);
        let desktop = Probe::default();
        placed(QuestionPlacement::Focused, &desktop);
        assert_eq!(desktop.reads(), [1, 1, 0, 0]);
    }

    /// R12.1 S10-S17: every notification state, and a failed query. Only a
    /// full-screen game or a presentation filling the focused window's
    /// monitor moves the question: to the pointer's monitor, else the
    /// notification display, else (game only) the first other display.
    #[test]
    fn a_covered_display_hands_the_question_elsewhere() {
        use NotificationState::*;
        let case = |state, fills, foreground, pointer, notification_on| {
            let probe = Probe {
                foreground: Some(foreground),
                pointer,
                state,
                fills,
                ..Default::default()
            };
            place(QuestionPlacement::Focused, &probe, &layout(notification_on))
        };
        let d = |n: &str| Some(n.to_owned());
        // States that never move the question, even full-screen.
        for state in [
            Some(NotPresent),
            Some(Busy),
            Some(AcceptsNotifications),
            Some(QuietTime),
            Some(App),
            None,
        ] {
            assert_eq!(
                case(state, true, "DISPLAY2", Some("DISPLAY3"), None),
                d("DISPLAY2"),
                "{state:?}"
            );
        }
        for state in [RunningD3dFullScreen, PresentationMode] {
            // Not filling its monitor (F11-like windowed, presentation mode
            // with an ordinary window): stays.
            assert_eq!(
                case(Some(state), false, "DISPLAY2", Some("DISPLAY3"), None),
                d("DISPLAY2")
            );
            // Covered: the pointer's monitor when it differs.
            assert_eq!(
                case(Some(state), true, "DISPLAY2", Some("DISPLAY3"), None),
                d("DISPLAY3")
            );
            // Else the notification display (the primary, `None`).
            assert_eq!(
                case(Some(state), true, "DISPLAY2", Some("DISPLAY2"), None),
                None
            );
        }
        // The notification display itself covered, pointer on it too: a game
        // takes the first other display, primary first; a presentation stays.
        assert_eq!(
            case(
                Some(RunningD3dFullScreen),
                true,
                "DISPLAY1",
                Some("DISPLAY1"),
                None
            ),
            d("DISPLAY2")
        );
        assert_eq!(
            case(
                Some(PresentationMode),
                true,
                "DISPLAY1",
                Some("DISPLAY1"),
                None
            ),
            None
        );
        // With the notification area on DISPLAY2, a covered DISPLAY2 game
        // goes to the primary first.
        assert_eq!(
            case(
                Some(RunningD3dFullScreen),
                true,
                "DISPLAY2",
                Some("DISPLAY2"),
                Some("DISPLAY2")
            ),
            d("DISPLAY1")
        );
    }

    /// Tier W: the real Win32 reads on a single-display runner resolve every
    /// placement to that display (the notification display, `None`).
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "tier W: real desktop reads; run by the informational windows.yml step"]
    fn windows_single_display_placement_resolves_to_that_display() {
        let probe = Win32Probe;
        let pointer = probe.pointer_display().expect("the pointer has a monitor");
        let layout = DisplayLayout::new(
            vec![DisplayRect {
                name: pointer.clone(),
                rect: Rect::new(0.0, 0.0, 1920.0, 1080.0),
                primary: true,
            }],
            &Default::default(),
        );
        eprintln!(
            "pointer {pointer}; foreground {:?}; state {:?}",
            probe.foreground(),
            probe.notification_state()
        );
        for placement in QuestionPlacement::ALL {
            assert_eq!(place(placement, &probe, &layout), None, "{placement:?}");
        }
    }

    /// The live setting is read once per placement; a change applies to the
    /// next question only.
    #[test]
    fn the_placer_follows_the_live_setting() {
        let defaults = PublicationDefaults::default();
        let probe = Arc::new(Probe {
            foreground: Some("DISPLAY2"),
            pointer: Some("DISPLAY3"),
            state: Some(NotificationState::AcceptsNotifications),
            ..Default::default()
        });
        let placer =
            DisplayPlacer::new(defaults.clone(), probe, Arc::new(Mutex::new(layout(None))));
        assert_eq!(placer.place(), Some("DISPLAY2".into()));
        defaults.set_question_placement(QuestionPlacement::Pointer);
        assert_eq!(placer.place(), Some("DISPLAY3".into()));
        defaults.set_question_placement(QuestionPlacement::Fixed);
        assert_eq!(placer.place(), None);
    }
}
