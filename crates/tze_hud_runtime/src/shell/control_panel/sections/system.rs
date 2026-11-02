//! System: restart (applying saved restart-only settings), install an
//! update, start with Windows, open the config file and the log folder, and
//! which build is running. Version and the restart/update outcomes are the
//! facts `/admin/status` reports.

use std::sync::{Arc, Mutex};

use tze_hud_compositor::{PanelBadge, PanelTone};

use super::super::row::{BoxFuture, Control, Edit, Row, Section};
use super::super::runtime::RuntimeControl;
use crate::launch::Source;
use crate::operator::handoff::RestartError;
use crate::operator::status::build_label;
use crate::operator::update::Outcome;

pub(crate) struct SystemSection {
    /// `None`: only the version shows (no runtime to act on).
    control: Option<RuntimeControl>,
    /// What the last Install update did, until the panel closes.
    note: Mutex<Option<String>>,
    /// Start-at-sign-in as last read (`refresh`).
    autostart: Arc<Mutex<Option<Result<bool, String>>>>,
    /// When the panel last read it.
    checked: Mutex<Option<std::time::Instant>>,
}

/// What this instance was launched with and cannot change while it runs: the
/// display profile (`[runtime].profile`, which config must set) and the
/// window. Each names where its value came from; change them in the config
/// file or the launch arguments, then restart.
fn launch_rows(control: &RuntimeControl) -> Vec<Row> {
    let mut rows = Vec::new();
    if let Some(profile) = &control.profile {
        rows.push(
            Row::new(
                SystemSection::PROFILE,
                "Display profile",
                Control::ReadOnly {
                    text: profile.clone(),
                    mono: true,
                },
            )
            .detail(Source::Config.describe())
            .keywords(&["profile", "runtime.profile", "budget"]),
        );
    }
    let sources = control.sources();
    // A size names its source only where the window uses it.
    let size = if control.window_sized {
        [sources.window_width, sources.window_height]
    } else {
        [Source::Default; 2]
    };
    let pinned: Vec<String> = [sources.window_mode]
        .into_iter()
        .chain(size)
        .filter(|s| s.is_pinned())
        .map(Source::describe)
        .collect();
    let source = if pinned.is_empty() {
        Source::Default.describe()
    } else {
        pinned.join(" · ")
    };
    rows.push(
        Row::new(
            SystemSection::WINDOW,
            "Window",
            Control::ReadOnly {
                text: control.window.clone(),
                mono: false,
            },
        )
        .detail(source)
        .keywords(&[
            "window",
            "mode",
            "overlay",
            "fullscreen",
            "size",
            "width",
            "height",
        ]),
    );
    rows
}

impl SystemSection {
    pub(crate) const RESTART: &'static str = "system.restart";
    pub(crate) const UPDATE: &'static str = "system.update";
    pub(crate) const AUTOSTART: &'static str = "system.autostart";
    pub(crate) const OPEN_CONFIG: &'static str = "system.open_config";
    pub(crate) const OPEN_LOGS: &'static str = "system.open_logs";
    pub(crate) const PROFILE: &'static str = "system.profile";
    pub(crate) const WINDOW: &'static str = "system.window";

    pub(crate) fn new(control: Option<RuntimeControl>) -> Self {
        Self {
            control,
            note: Mutex::default(),
            autostart: Arc::default(),
            checked: Mutex::default(),
        }
    }

    fn rows_for(&self, control: &RuntimeControl) -> Vec<Row> {
        let mut rows = Vec::new();
        if let Some(restart) = &control.restart {
            let pending = control.pending();
            let last_error = restart.last_json()["error"].as_str().map(str::to_owned);
            // A failed restart left this instance up: say why first.
            let detail = match (last_error, pending.len()) {
                (Some(error), _) => format!("Last restart failed: {error}"),
                (None, 0) => "Agent content clears; agents reconnect".to_owned(),
                (None, 1) => "Applies 1 change · agent content clears".to_owned(),
                (None, n) => format!("Applies {n} changes · agent content clears"),
            };
            rows.push(
                Row::new(
                    Self::RESTART,
                    "Restart now",
                    Control::Action {
                        confirm: Some(
                            "Restart the HUD now? Agents' content clears and they reconnect."
                                .into(),
                        ),
                    },
                )
                .detail(detail)
                .keywords(&["restart", "relaunch", "apply", "reload"])
                .badge((!pending.is_empty()).then(|| PanelBadge {
                    text: pending.join(", "),
                    tone: PanelTone::Caution,
                })),
            );
        }
        if let Some(update) = &control.update {
            let note = self.note.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let failed = update.last_json()["error"].as_str().map(str::to_owned);
            let detail = note
                .or_else(|| failed.map(|e| format!("Last update failed: {e}")))
                .unwrap_or_else(|| format!("{} channel · signed builds only", control.channel));
            rows.push(
                Row::new(
                    Self::UPDATE,
                    "Install update",
                    Control::Action {
                        confirm: Some(format!(
                            "Install the latest signed {} build? The HUD restarts.",
                            control.channel
                        )),
                    },
                )
                .detail(detail)
                .keywords(&["update", "upgrade", "release", "channel"]),
            );
        }
        if control.autostart.is_some() {
            let state = self
                .autostart
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let row = match state {
                Some(Ok(on)) => {
                    Row::new(Self::AUTOSTART, "Start with Windows", Control::Toggle(on))
                        .detail("Starts the HUD when you sign in")
                }
                Some(Err(error)) => Row::new(
                    Self::AUTOSTART,
                    "Start with Windows",
                    Control::ReadOnly {
                        text: "Unknown".into(),
                        mono: false,
                    },
                )
                .detail(error),
                None => Row::new(
                    Self::AUTOSTART,
                    "Start with Windows",
                    Control::ReadOnly {
                        text: "Checking…".into(),
                        mono: false,
                    },
                ),
            };
            rows.push(row.keywords(&["autostart", "startup", "login", "sign in", "boot"]));
        }
        if let Some(path) = &control.config_path {
            rows.push(
                Row::new(
                    Self::OPEN_CONFIG,
                    "Open config file",
                    Control::Action { confirm: None },
                )
                .detail(path.display().to_string())
                .keywords(&["config", "toml", "settings", "file", "edit"]),
            );
        }
        rows.push(
            Row::new(
                Self::OPEN_LOGS,
                "Open log folder",
                Control::Action { confirm: None },
            )
            .detail(control.log_dir.display().to_string())
            .keywords(&["logs", "log", "folder", "diagnostics"]),
        );
        rows.extend(launch_rows(control));
        rows
    }

    async fn install_update(&self, control: &RuntimeControl) -> Result<(), String> {
        let update = control.update.clone().ok_or("No updater in this build")?;
        let channel = control.channel.clone();
        let outcome = tokio::task::spawn_blocking(move || update.request(&channel))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.hint().to_owned())?;
        let note = match outcome {
            Outcome::UpToDate => "Already up to date".to_owned(),
            Outcome::Started { sha } => {
                format!("Installing {}…", sha.get(..7).unwrap_or(&sha))
            }
        };
        tracing::info!(%note, "control panel: update requested");
        *self.note.lock().unwrap_or_else(|e| e.into_inner()) = Some(note);
        Ok(())
    }

    async fn set_autostart(&self, control: &RuntimeControl, on: bool) -> Result<(), String> {
        let autostart = control.autostart.clone().ok_or("Not installed")?;
        let state = tokio::task::spawn_blocking(move || autostart(Some(on)))
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(on, ok = state.is_ok(), "control panel: autostart changed");
        *self.autostart.lock().unwrap_or_else(|e| e.into_inner()) = Some(state.clone());
        state.map(drop)
    }
}

impl Section for SystemSection {
    fn id(&self) -> &'static str {
        "system"
    }

    fn summary(&self) -> Option<String> {
        let control = self.control.as_ref()?;
        // Also without Restart now: the owner must then restart by hand.
        (!control.pending().is_empty()).then(|| "restart pending".to_owned())
    }

    fn title(&self) -> &'static str {
        "System"
    }

    fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
        Box::pin(async move {
            let mut rows = self
                .control
                .as_ref()
                .map(|control| self.rows_for(control))
                .unwrap_or_default();
            rows.push(
                Row::new(
                    "system.version",
                    "Version",
                    Control::ReadOnly {
                        text: build_label(),
                        mono: true,
                    },
                )
                .keywords(&["build", "channel", "sha", "release"]),
            );
            rows
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let control = self.control.as_ref().ok_or("Read-only")?;
            match (row, edit) {
                (Self::RESTART, Edit::Activate) => {
                    let restart = control.restart.as_ref().ok_or("Restart is unavailable")?;
                    tracing::info!(pending = ?control.pending(), "control panel: restart requested");
                    restart.request().map_err(|e| match e {
                        RestartError::Busy => "A restart or update is already running".to_owned(),
                        RestartError::Unavailable => "Could not start the restart".to_owned(),
                    })
                }
                (Self::UPDATE, Edit::Activate) => self.install_update(control).await,
                (Self::AUTOSTART, Edit::Set(on)) => self.set_autostart(control, on).await,
                (Self::OPEN_CONFIG, Edit::Activate) => {
                    let path = control.config_path.as_ref().ok_or("No config file")?;
                    (control.open)(path)
                }
                (Self::OPEN_LOGS, Edit::Activate) => {
                    std::fs::create_dir_all(&control.log_dir).map_err(|e| e.to_string())?;
                    (control.open)(&control.log_dir)
                }
                (_, edit) => Err(format!("{row} does not support {edit:?}")),
            }
        })
    }

    fn opened(&self) {
        *self.checked.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn refresh(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let Some(autostart) = self.control.as_ref().and_then(|c| c.autostart.clone()) else {
                return;
            };
            if !super::due(&self.checked) {
                return;
            }
            let state = tokio::task::spawn_blocking(move || autostart(None))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            *self.autostart.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::shell::control_panel::runtime::tests::fake;

    fn launch(control: &RuntimeControl) -> Vec<(String, String, Option<String>)> {
        launch_rows(control)
            .into_iter()
            .map(|row| {
                let Control::ReadOnly { text, .. } = row.control else {
                    panic!("{} is read-only", row.id)
                };
                (row.id.to_string(), text, row.detail)
            })
            .collect()
    }

    /// The profile and the window show read-only, each with where its value
    /// came from (CP: every launch tunable is visible).
    #[test]
    fn launch_rows_show_profile_and_window_with_their_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut control = fake(
            &dir.path().join("tze_hud.toml"),
            Arc::new(Mutex::new(Vec::new())),
            None,
        );
        assert_eq!(
            launch(&control),
            [
                (
                    "system.profile".into(),
                    "full-display".into(),
                    Some("Saved to config".into())
                ),
                (
                    "system.window".into(),
                    "Fullscreen".into(),
                    Some("Default".into())
                ),
            ]
        );

        {
            let mut sources = control.sources.lock().unwrap();
            sources.window_mode = Source::Flag("--window-mode");
            sources.window_width = Source::Env("TZE_HUD_WINDOW_WIDTH");
            sources.window_height = Source::Flag("--height");
        }
        assert_eq!(
            launch(&control)[1].2.as_deref(),
            Some("Set by --window-mode"),
            "fullscreen ignores the size, so its sources go unnamed"
        );
        control.window = "Overlay · 1280×720".into();
        control.window_sized = true;
        control.profile = None;
        assert_eq!(
            launch(&control),
            [(
                "system.window".into(),
                "Overlay · 1280×720".into(),
                Some("Set by --window-mode · Set by TZE_HUD_WINDOW_WIDTH · Set by --height".into())
            )],
            "no profile in config: no row rather than a guess"
        );
    }

    /// The header says "restart pending" while a saved port differs from
    /// the running one, also when this copy has no Restart now (the owner
    /// then restarts by hand).
    #[test]
    fn summary_flags_a_pending_restart_without_restart_now() {
        let dir = tempfile::tempdir().unwrap();
        let control = fake(
            &dir.path().join("tze_hud.toml"),
            Arc::new(Mutex::new(Vec::new())),
            None,
        );
        assert!(control.restart.is_none());
        let section = SystemSection::new(Some(control.clone()));
        assert_eq!(section.summary(), None);
        control.saved.lock().unwrap().grpc = 50061;
        assert_eq!(section.summary().as_deref(), Some("restart pending"));
    }
}
