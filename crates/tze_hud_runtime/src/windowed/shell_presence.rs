//! Windows desktop presence. The existing display-watcher window owns native
//! resources; the network-runtime panel task owns every command's effects.

use serde::Serialize;
use tze_hud_config::tokens::DesignTokenMap;
use tze_hud_scene::SceneGraph;
use tze_hud_scene::graph::SurfaceClasses;

#[cfg(any(target_os = "windows", test))]
use crate::shell::control_panel::TrayFocusOrigin;

#[cfg(target_os = "windows")]
pub(super) mod native;

#[cfg(target_os = "windows")]
pub(crate) fn tray_status() -> serde_json::Value {
    native::status()
}

#[cfg(target_os = "windows")]
pub(crate) fn session_locked() -> bool {
    native::session_locked()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GlyphState {
    Normal,
    Waiting,
    Hidden,
    Paused,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Inputs {
    pub paused: bool,
    pub hidden: SurfaceClasses,
    pub visible_questions: usize,
    /// Delivery (d) will supply lock-held questions through this same seam.
    pub held_questions: usize,
    /// The panel header's connected-or-showing set, already sorted by id.
    pub agents: Vec<String>,
}

/// Count unanswered cards once, regardless of how many callback buttons each
/// card contains. Hidden cards still wait for an answer; URL-only cards do not.
fn question_count(scene: &SceneGraph) -> usize {
    scene
        .zone_registry
        .active_publishes
        .values()
        .flatten()
        .filter(|record| scene.is_unanswered_question(record))
        .count()
}

/// What a left click on the icon does now (`/admin/status` `tray.click`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TrayClick {
    /// Focus the newest question, as the answer chord does.
    Answer,
    /// Open or close the control panel.
    Panel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Presentation {
    pub glyph: GlyphState,
    pub tooltip: String,
    pub all_hidden: bool,
    pub click: TrayClick,
}

impl From<&Inputs> for Presentation {
    fn from(input: &Inputs) -> Self {
        let waiting = input.visible_questions.saturating_add(input.held_questions);
        let any_hidden = input.hidden.portals || input.hidden.zones || input.hidden.widgets;
        let all_hidden = input.hidden.portals && input.hidden.zones && input.hidden.widgets;
        let glyph = if input.paused {
            GlyphState::Paused
        } else if any_hidden {
            GlyphState::Hidden
        } else if waiting > 0 {
            GlyphState::Waiting
        } else {
            GlyphState::Normal
        };
        // A shown question card with agents running: the chord's target.
        // A question that goes before the click lands opens the panel.
        let click = if !input.paused && !input.hidden.zones && input.visible_questions > 0 {
            TrayClick::Answer
        } else {
            TrayClick::Panel
        };
        let state = if input.paused {
            "Paused".to_owned()
        } else if all_hidden {
            "Everything hidden".to_owned()
        } else {
            let count = match waiting {
                0 => "No questions".to_owned(),
                1 => "1 question waiting".to_owned(),
                n => format!("{n} questions waiting"),
            };
            match click {
                TrayClick::Answer => format!("{count} · click to answer"),
                TrayClick::Panel => count,
            }
        };
        let agents = if input.agents.is_empty() {
            "No agents".to_owned()
        } else {
            // A namespace cannot introduce a third tooltip line or a NUL.
            input.agents.join(", ").replace(['\r', '\n', '\0'], " ")
        };
        let remaining = 127 - state.encode_utf16().count() - 1;
        Self {
            glyph,
            tooltip: format!("{state}\n{}", ellipsize(&agents, remaining)),
            all_hidden,
            click,
        }
    }
}

/// Preserve complete Unicode scalars at the shell's UTF-16 limit, reserving
/// space for the ellipsis itself. szTip's final NUL is the 128th unit.
fn ellipsize(text: &str, units: usize) -> String {
    if text.encode_utf16().count() <= units {
        return text.to_owned();
    }
    let mut remaining = units.saturating_sub(1);
    let mut result = String::new();
    for c in text.chars() {
        if c.len_utf16() > remaining {
            break;
        }
        result.push(c);
        remaining -= c.len_utf16();
    }
    if units > 0 {
        result.push('…');
    }
    result
}

#[cfg(any(target_os = "windows", test))]
use crate::shell::control_panel::TrayCommand;

/// The tray menu in order, each label followed by a tab and its chord when
/// that chord is registered, so Windows shows it in the shortcut column
/// Read at each menu open; nothing runs while it is closed.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn menu_items(
    view: &Presentation,
    hotkey: impl Fn(tze_hud_scene::config::ChordRole) -> crate::operator::status::HotkeyStatus,
) -> Vec<(TrayCommand, String)> {
    use tze_hud_scene::config::ChordRole;
    let paused = view.glyph == GlyphState::Paused;
    let items = [
        (view.click == TrayClick::Answer).then_some((
            TrayCommand::AnswerNext,
            "&Answer next question",
            Some(ChordRole::Answer),
        )),
        Some((
            TrayCommand::OpenPanel,
            "&Open panel",
            Some(ChordRole::Panel),
        )),
        Some(if paused {
            (
                TrayCommand::Resume,
                "&Resume agents",
                Some(ChordRole::Pause),
            )
        } else {
            (TrayCommand::Pause, "&Pause agents", Some(ChordRole::Pause))
        }),
        Some(if view.all_hidden {
            (TrayCommand::ShowAll, "&Show all", Some(ChordRole::Hide))
        } else {
            (TrayCommand::HideAll, "&Hide all", Some(ChordRole::Hide))
        }),
        Some((TrayCommand::Quit, "&Quit", None)),
    ];
    items
        .into_iter()
        .flatten()
        .map(|(command, text, role)| {
            let label = match role.map(&hotkey) {
                Some(crate::operator::status::HotkeyStatus::Registered { chord }) => {
                    format!("{text}\t{chord}")
                }
                _ => text.to_owned(),
            };
            (command, label)
        })
        .collect()
}

/// One top-level window as the tray click saw it.
#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Debug, Default)]
pub(crate) struct WindowFacts {
    pub hwnd: isize,
    pub visible: bool,
    pub minimized: bool,
    pub cloaked: bool,
    pub topmost: bool,
    pub tool: bool,
    pub owned: bool,
    pub hud: bool,
    pub class: String,
}

#[cfg(any(target_os = "windows", test))]
fn is_tray_shell_window(class: &str) -> bool {
    matches!(
        class,
        "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Progman" | "WorkerW"
    )
}

#[cfg(any(target_os = "windows", test))]
fn tray_focus_origin(
    foreground: Option<WindowFacts>,
    fallback: impl FnOnce() -> Option<isize>,
) -> TrayFocusOrigin {
    match foreground {
        Some(window) if window.hud => TrayFocusOrigin::Preserve,
        Some(window) if is_tray_shell_window(&window.class) => TrayFocusOrigin::Replace(fallback()),
        Some(window) => TrayFocusOrigin::Replace(Some(window.hwnd)),
        None => TrayFocusOrigin::Replace(None),
    }
}

/// When the shell has already taken focus, look behind it once in z-order.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn return_window(windows: impl IntoIterator<Item = WindowFacts>) -> Option<isize> {
    windows
        .into_iter()
        .find(|w| {
            w.visible
                && !w.minimized
                && !w.cloaked
                && !w.topmost
                && !w.tool
                && !w.owned
                && !w.hud
                && !is_tray_shell_window(&w.class)
        })
        .map(|w| w.hwnd)
}

/// The four shell operations form the native I/O seam. Keeping registration
/// transitions here lets failure/restart/teardown run against the same logic
/// on headless hosts, without pretending that a fake proves Explorer behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShellOperation {
    Add,
    Version4,
    Modify,
    Delete,
}

pub(super) trait ShellIcon {
    fn call(&mut self, operation: ShellOperation) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Registration {
    pub registered: bool,
    attempted: bool,
    stopped: bool,
}

impl Registration {
    pub(super) fn apply(
        &mut self,
        restart: bool,
        changed: bool,
        shell: &mut impl ShellIcon,
    ) -> bool {
        if self.stopped {
            return false;
        }
        if restart || !self.attempted {
            self.attempted = true;
            self.registered = shell.call(ShellOperation::Add);
            if self.registered && !shell.call(ShellOperation::Version4) {
                shell.call(ShellOperation::Delete);
                self.registered = false;
            }
        } else if self.registered && changed {
            self.registered = shell.call(ShellOperation::Modify);
        }
        self.registered
    }

    pub(super) fn stop(&mut self, shell: &mut impl ShellIcon) {
        if !self.stopped {
            self.stopped = true;
            if self.attempted {
                shell.call(ShellOperation::Delete);
            }
            self.registered = false;
        }
    }
}

/// Track lock and connection independently: a connect event alone must not
/// unlock a locked session, nor may unlock expose a disconnected session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionState {
    pub locked: bool,
    pub disconnected: bool,
}

impl SessionState {
    pub(crate) fn held(self) -> bool {
        self.locked || self.disconnected
    }

    pub(crate) fn event(&mut self, code: u32) -> bool {
        let before = self.held();
        match code {
            1 | 3 => self.disconnected = false, // console / remote connect
            2 | 4 => self.disconnected = true,  // console / remote disconnect
            7 => self.locked = true,
            8 => self.locked = false,
            _ => {}
        }
        before != self.held()
    }
}

/// Only these resolved canonical tokens affect the tray. HUD scale, opacity,
/// font or unrelated token changes do not trigger another rasterization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GlyphColors {
    on_light: String,
    on_dark: String,
    caution: String,
    outline: String,
    primary: String,
}

impl GlyphColors {
    pub(crate) fn from_tokens(tokens: &DesignTokenMap) -> Self {
        let color = |key: &str| tokens.get(key).expect("resolved canonical token").clone();
        Self {
            on_light: color("tray.glyph.on_light"),
            on_dark: color("tray.glyph.on_dark"),
            caution: color("color.caution"),
            outline: color("color.outline"),
            primary: color("color.primary"),
        }
    }

    pub(crate) fn svg(&self, state: GlyphState, light_taskbar: bool) -> String {
        let base = if light_taskbar {
            &self.on_light
        } else {
            &self.on_dark
        };
        let (glyph, mark) = match state {
            GlyphState::Normal => (base, String::new()),
            GlyphState::Hidden => (&self.outline, String::new()),
            GlyphState::Waiting => (
                base,
                format!(
                    "<circle cx=\"19\" cy=\"18\" r=\"4\" fill=\"{}\"/>",
                    self.primary
                ),
            ),
            GlyphState::Paused => (
                base,
                format!(
                    "<path d=\"M14 13h3v9h-3zm5 0h3v9h-3z\" fill=\"{}\"/>",
                    self.caution
                ),
            ),
        };
        include_str!("../../../../assets/tray.svg")
            .replace("{{glyph}}", glyph)
            .replace("{{mark}}", &mark)
    }
}

fn rasterize(svg: &str, size: u32) -> Result<resvg::tiny_skia::Pixmap, String> {
    let tree = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default())
        .map_err(|e| e.to_string())?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).ok_or("tray pixmap allocation")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(
            size as f32 / tree.size().width(),
            size as f32 / tree.size().height(),
        ),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator::status::HotkeyStatus;
    use tze_hud_scene::ZoneContent;

    #[test]
    fn tray_presentation_and_session_transitions() {
        use std::sync::Arc;
        use tze_hud_scene::clock::TestClock;
        use tze_hud_scene::{NotificationAction, NotificationPayload};

        let clock = Arc::new(TestClock::new(100));
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
        scene.register_zone(crate::windowed::test_support::make_test_zone("questions"));
        let callback = NotificationAction {
            label: "Answer".into(),
            callback_id: Some("answer".into()),
            url: None,
            role: None,
        };
        for actions in [
            vec![callback.clone(), callback.clone()],
            vec![callback],
            vec![NotificationAction {
                label: "Open link".into(),
                callback_id: None,
                url: Some("https://example.com".into()),
                role: None,
            }],
        ] {
            scene
                .publish_to_zone(
                    "questions",
                    ZoneContent::Notification(NotificationPayload {
                        text: "A question".into(),
                        actions,
                        ..Default::default()
                    }),
                    "agent",
                    None,
                    Some(200_000),
                    None,
                )
                .unwrap();
        }
        assert_eq!(
            question_count(&scene),
            2,
            "count cards, not buttons or links"
        );
        let answered = scene.zone_registry.active_publishes["questions"][0].publication_id;
        scene
            .overlay
            .notification_choices
            .insert(answered, "answer".into());
        assert_eq!(question_count(&scene), 1, "chosen feedback is not waiting");
        clock.set(200);
        assert_eq!(
            question_count(&scene),
            0,
            "expiry precedes physical reaping"
        );

        let mut input = Inputs::default();
        assert_eq!(Presentation::from(&input).glyph, GlyphState::Normal);
        assert_eq!(
            Presentation::from(&input).tooltip,
            "No questions\nNo agents"
        );
        input.visible_questions = 1;
        input.held_questions = 1;
        input.agents = vec!["connected".into(), "showing".into()];
        let view = Presentation::from(&input);
        assert_eq!(view.glyph, GlyphState::Waiting);
        assert_eq!(
            view.tooltip,
            "2 questions waiting · click to answer\nconnected, showing"
        );
        input.hidden = SurfaceClasses::ALL;
        assert_eq!(Presentation::from(&input).glyph, GlyphState::Hidden);
        assert!(Presentation::from(&input).all_hidden);
        input.paused = true;
        input.agents = vec!["🦀".repeat(100)];
        let view = Presentation::from(&input);
        assert_eq!(view.glyph, GlyphState::Paused);
        assert!(view.tooltip.starts_with("Paused\n"));
        assert!(view.tooltip.ends_with('…'));
        let units: Vec<_> = view.tooltip.encode_utf16().collect();
        assert!(units.len() <= 127);
        assert!(String::from_utf16(&units).is_ok());

        struct Shell {
            calls: Vec<ShellOperation>,
            succeeds: bool,
        }
        impl ShellIcon for Shell {
            fn call(&mut self, operation: ShellOperation) -> bool {
                self.calls.push(operation);
                self.succeeds
            }
        }
        use ShellOperation::{Add, Delete, Modify, Version4};
        let mut shell = Shell {
            calls: Vec::new(),
            succeeds: true,
        };
        let mut registration = Registration::default();
        assert!(registration.apply(false, false, &mut shell));
        assert!(registration.apply(false, false, &mut shell));
        assert_eq!(
            shell.calls,
            [Add, Version4],
            "one icon, VERSION_4, no idle shell work"
        );
        assert!(registration.apply(false, true, &mut shell));
        assert_eq!(shell.calls.last(), Some(&Modify));
        shell.succeeds = false;
        assert!(!registration.apply(true, false, &mut shell));
        let failed_calls = shell.calls.len();
        shell.succeeds = true;
        assert!(!registration.apply(false, true, &mut shell));
        assert_eq!(
            shell.calls.len(),
            failed_calls,
            "failed add waits for TaskbarCreated"
        );
        assert!(registration.apply(true, false, &mut shell));
        assert_eq!(&shell.calls[shell.calls.len() - 2..], &[Add, Version4]);
        registration.stop(&mut shell);
        let stopped_calls = shell.calls.len();
        registration.stop(&mut shell);
        assert!(!registration.apply(true, true, &mut shell));
        assert_eq!(shell.calls.last(), Some(&Delete));
        assert_eq!(
            shell.calls.len(),
            stopped_calls,
            "idempotent clean teardown"
        );

        let mut session = SessionState::default();
        for (code, held) in [
            (7, true),
            (2, true),
            (8, true),
            (1, false),
            (4, true),
            (3, false),
        ] {
            session.event(code);
            assert_eq!(session.held(), held, "WTS code {code}");
        }
        assert!(!session.event(99));
        // Ids name commands, so the answer item cannot shift the others.
        for (command, _) in menu_items(&view, |_| HotkeyStatus::NotApplicable) {
            let id = u32::try_from(command.menu_id()).unwrap();
            assert_ne!(id, 0, "0 is a dismissed menu");
            assert_eq!(TrayCommand::from_menu_id(id), Some(command));
        }
        assert_eq!(TrayCommand::from_menu_id(0), None);
        assert_eq!(TrayCommand::from_menu_id(8), None);
        assert_eq!(TrayCommand::TogglePanel.menu_id(), 0, "never a menu item");
    }

    /// A click answers only a shown question with agents
    /// running; the tooltip says so within the shell's limit.
    #[test]
    fn tray_click_answers_only_a_shown_question_with_agents_running() {
        let question = Inputs {
            visible_questions: 2,
            agents: vec!["ops".into()],
            ..Default::default()
        };
        let view = Presentation::from(&question);
        assert_eq!(view.click, TrayClick::Answer);
        assert_eq!(view.tooltip, "2 questions waiting · click to answer\nops");
        assert_eq!(
            serde_json::to_value(view.click).unwrap(),
            serde_json::json!("answer")
        );
        let cases = [
            ("no question", Inputs::default()),
            (
                "paused",
                Inputs {
                    paused: true,
                    ..question.clone()
                },
            ),
            (
                "zones hidden, question still counted",
                Inputs {
                    hidden: SurfaceClasses {
                        zones: true,
                        ..Default::default()
                    },
                    ..question.clone()
                },
            ),
            (
                "only a lock-held question",
                Inputs {
                    visible_questions: 0,
                    held_questions: 1,
                    ..question.clone()
                },
            ),
        ];
        for (case, input) in cases {
            let view = Presentation::from(&input);
            assert_eq!(view.click, TrayClick::Panel, "{case}");
            assert!(!view.tooltip.contains("click to answer"), "{case}");
            assert_eq!(
                serde_json::to_value(view.click).unwrap(),
                serde_json::json!("panel")
            );
        }
        // Portals or widgets hidden alone leave the cards drawn.
        let portals_hidden = Inputs {
            hidden: SurfaceClasses {
                portals: true,
                widgets: true,
                ..Default::default()
            },
            ..question.clone()
        };
        assert_eq!(Presentation::from(&portals_hidden).click, TrayClick::Answer);

        // The suffix counts toward the 127-unit limit; names give way.
        let long = Inputs {
            visible_questions: 12,
            agents: vec!["🦀".repeat(100)],
            ..Default::default()
        };
        let view = Presentation::from(&long);
        let units: Vec<_> = view.tooltip.encode_utf16().collect();
        assert!(units.len() <= 127, "{} units", units.len());
        assert!(String::from_utf16(&units).is_ok());
        assert!(
            view.tooltip
                .starts_with("12 questions waiting · click to answer\n")
        );
        assert!(view.tooltip.ends_with('…'));
        // Nothing changes, nothing differs: the bridge posts no refresh.
        assert_eq!(Presentation::from(&long), view);
    }

    /// Each item shows its registered chord after a tab.
    #[test]
    fn tray_menu_labels_show_registered_chords() {
        use tze_hud_scene::config::ChordRole;
        let defaults = |role: ChordRole| HotkeyStatus::Registered {
            chord: match role {
                ChordRole::Pause => "Ctrl+Shift+F12",
                ChordRole::Panel => "Ctrl+Shift+F11",
                ChordRole::Pairing => "Ctrl+Shift+F10",
                ChordRole::Answer => "Ctrl+Shift+F9",
                ChordRole::Hide => "Ctrl+Shift+F8",
            }
            .into(),
        };
        let labels = |input: &Inputs, hotkey: &dyn Fn(ChordRole) -> HotkeyStatus| {
            menu_items(&Presentation::from(input), hotkey)
        };
        let idle = Inputs::default();
        assert_eq!(
            labels(&idle, &defaults),
            [
                (TrayCommand::OpenPanel, "&Open panel\tCtrl+Shift+F11".into()),
                (TrayCommand::Pause, "&Pause agents\tCtrl+Shift+F12".into()),
                (TrayCommand::HideAll, "&Hide all\tCtrl+Shift+F8".into()),
                (TrayCommand::Quit, "&Quit".to_owned()),
            ]
        );
        let question = Inputs {
            visible_questions: 1,
            ..Default::default()
        };
        let items = labels(&question, &defaults);
        assert_eq!(
            items[0],
            (
                TrayCommand::AnswerNext,
                "&Answer next question\tCtrl+Shift+F9".into()
            ),
            "first, and so the default item"
        );
        assert_eq!(items.len(), 5);
        // Flipped labels keep the same chords.
        let flipped = Inputs {
            paused: true,
            hidden: SurfaceClasses::ALL,
            ..Default::default()
        };
        assert_eq!(
            labels(&flipped, &defaults)[1..3],
            [
                (TrayCommand::Resume, "&Resume agents\tCtrl+Shift+F12".into()),
                (TrayCommand::ShowAll, "&Show all\tCtrl+Shift+F8".into()),
            ]
        );
        // A rebound chord shows its new keys at the next open.
        let rebound = |role| match role {
            ChordRole::Answer => HotkeyStatus::Registered {
                chord: "Ctrl+Alt+F7".into(),
            },
            role => defaults(role),
        };
        assert_eq!(items[0].1, "&Answer next question\tCtrl+Shift+F9");
        assert_eq!(
            labels(&question, &rebound)[0].1,
            "&Answer next question\tCtrl+Alt+F7"
        );
        // Pending, refused and unbound chords show nothing; items remain.
        let unregistered = |role| match role {
            ChordRole::Answer => HotkeyStatus::Pending {
                chord: "Ctrl+Shift+F9".into(),
            },
            ChordRole::Pause => HotkeyStatus::Failed {
                chord: "Ctrl+Shift+F12".into(),
                reason: "held by another program".into(),
            },
            ChordRole::Hide => HotkeyStatus::NotApplicable,
            role => defaults(role),
        };
        let bare = labels(&question, &unregistered);
        assert_eq!(
            bare.iter()
                .map(|(_, label)| label.as_str())
                .collect::<Vec<_>>(),
            [
                "&Answer next question",
                "&Open panel\tCtrl+Shift+F11",
                "&Pause agents",
                "&Hide all",
                "&Quit",
            ]
        );
        // Mnemonics stay distinct and before the tab.
        let mnemonics: std::collections::BTreeSet<_> = items
            .iter()
            .map(|(_, label)| {
                let (text, _) = label.split_once('\t').unwrap_or((label, ""));
                text.split_once('&').unwrap().1.chars().next().unwrap()
            })
            .collect();
        assert_eq!(mnemonics.len(), items.len());
    }

    /// The window-order pick, one exclusion each.
    #[test]
    fn tray_visit_returns_to_the_first_application_window() {
        let app = |hwnd| WindowFacts {
            hwnd,
            visible: true,
            class: "Notepad".into(),
            ..Default::default()
        };
        assert_eq!(return_window([app(1), app(2)]), Some(1), "top of z-order");
        type Exclude = fn(&mut WindowFacts);
        let exclusions: [(&str, Exclude); 11] = [
            ("hidden", |w| w.visible = false),
            ("minimised", |w| w.minimized = true),
            ("cloaked", |w| w.cloaked = true),
            ("topmost", |w| w.topmost = true),
            ("tool window", |w| w.tool = true),
            ("owned", |w| w.owned = true),
            ("HUD process", |w| w.hud = true),
            ("taskbar", |w| w.class = "Shell_TrayWnd".into()),
            ("second taskbar", |w| {
                w.class = "Shell_SecondaryTrayWnd".into()
            }),
            ("desktop", |w| w.class = "Progman".into()),
            ("wallpaper", |w| w.class = "WorkerW".into()),
        ];
        for (case, exclude) in exclusions {
            let mut first = app(1);
            exclude(&mut first);
            assert_eq!(return_window([first.clone(), app(2)]), Some(2), "{case}");
            assert_eq!(return_window([first]), None, "{case}: nothing raised");
        }
        assert_eq!(return_window([]), None);

        let mut modal = app(1);
        modal.topmost = true;
        modal.tool = true;
        modal.owned = true;
        assert_eq!(
            tray_focus_origin(Some(modal), || panic!("foreground was available")),
            TrayFocusOrigin::Replace(Some(1))
        );
        let mut shell = app(1);
        shell.class = "Shell_SecondaryTrayWnd".into();
        assert_eq!(
            tray_focus_origin(Some(shell), || return_window([app(2)])),
            TrayFocusOrigin::Replace(Some(2))
        );
        let mut hud = app(3);
        hud.hud = true;
        assert_eq!(
            tray_focus_origin(Some(hud), || panic!("preserve the existing HUD origin")),
            TrayFocusOrigin::Preserve
        );
        assert_eq!(
            tray_focus_origin(None, || panic!("no known shell foreground")),
            TrayFocusOrigin::Replace(None)
        );
    }

    #[test]
    fn tray_svg_uses_taskbar_contrast_and_semantic_state_tokens() {
        let tokens =
            tze_hud_config::tokens::resolve_tokens(&DesignTokenMap::new(), &DesignTokenMap::new());
        let colors = GlyphColors::from_tokens(&tokens);
        let light = colors.svg(GlyphState::Normal, true);
        let dark = colors.svg(GlyphState::Normal, false);
        assert!(light.contains(&tokens["tray.glyph.on_light"]));
        assert!(dark.contains(&tokens["tray.glyph.on_dark"]));
        let light_pixels = rasterize(&light, 32).unwrap();
        let dark_pixels = rasterize(&dark, 32).unwrap();
        assert_ne!(light_pixels.data(), dark_pixels.data());
        assert!(light_pixels.pixels().iter().any(|pixel| pixel.alpha() > 0));
        for (state, key) in [
            (GlyphState::Paused, "color.caution"),
            (GlyphState::Hidden, "color.outline"),
            (GlyphState::Waiting, "color.primary"),
        ] {
            let svg = colors.svg(state, false);
            assert!(svg.contains(&tokens[key]));
            assert!(!svg.contains("{{"));
            assert_ne!(rasterize(&svg, 32).unwrap().data(), dark_pixels.data());
        }
    }
}
