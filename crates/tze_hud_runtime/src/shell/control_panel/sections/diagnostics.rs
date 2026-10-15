//! Diagnostics: the log file's level, zone outlines and the frame rate cap.
//! Each applies at once and is saved to `[runtime]`; a value a flag or
//! variable set is shown read-only, naming it.

use tze_hud_config::{LOG_LEVELS, RuntimeUpdate};

use super::super::row::{BoxFuture, Control, Edit, Row, Section};
use super::super::runtime::RuntimeControl;
use crate::launch::Source;

/// Frame rate caps offered (the running one is added when it is not here).
const FPS_CHOICES: [u32; 6] = [30, 60, 90, 120, 144, 240];

pub(crate) struct DiagnosticsSection {
    control: RuntimeControl,
}

impl DiagnosticsSection {
    pub(crate) const LOG_LEVEL: &'static str = "diagnostics.log_level";
    pub(crate) const ZONE_OUTLINES: &'static str = "diagnostics.zone_outlines";
    pub(crate) const FPS: &'static str = "diagnostics.fps";

    pub(crate) fn new(control: RuntimeControl) -> Self {
        Self { control }
    }

    fn fps_options(&self) -> Vec<u32> {
        let mut options = FPS_CHOICES.to_vec();
        let current = self.control.live.fps();
        if !options.contains(&current) {
            options.push(current);
            options.sort_unstable();
        }
        options
    }

    /// A row a flag or variable set: its value, read-only, naming the source.
    fn pinned(id: &str, label: &str, text: String, source: Source) -> Row {
        Row::new(id, label, Control::ReadOnly { text, mono: true }).detail(source.describe())
    }

    /// Saved, or applied for now when it could not be saved.
    fn saved(&self, result: Result<(), String>) -> Result<(), String> {
        result.map_err(|e| format!("Applied until restart; not saved: {e}"))
    }
}

impl Section for DiagnosticsSection {
    fn id(&self) -> &'static str {
        "diagnostics"
    }

    fn title(&self) -> &'static str {
        "Diagnostics"
    }

    fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
        Box::pin(async move {
            let sources = self.control.sources();
            let level = (self.control.log_level.get)();
            let log_row = match level {
                None => Row::new(
                    Self::LOG_LEVEL,
                    "Log level",
                    Control::ReadOnly {
                        text: "No log file".into(),
                        mono: false,
                    },
                ),
                Some(level) if sources.log_level.is_pinned() => {
                    Self::pinned(Self::LOG_LEVEL, "Log level", level, sources.log_level)
                }
                Some(level) => Row::new(
                    Self::LOG_LEVEL,
                    "Log level",
                    Control::Choice {
                        options: LOG_LEVELS.iter().map(|l| (*l).to_owned()).collect(),
                        selected: LOG_LEVELS.iter().position(|l| *l == level).unwrap_or(2),
                    },
                )
                .detail(sources.log_level.describe()),
            }
            .keywords(&[
                "log",
                "logging",
                "verbose",
                "debug",
                "trace",
                "TZE_HUD_FILE_LOG",
            ]);

            let outlines = self.control.live.zone_outlines();
            let outlines_row = if sources.debug_zones.is_pinned() {
                Self::pinned(
                    Self::ZONE_OUTLINES,
                    "Zone outlines",
                    if outlines { "On" } else { "Off" }.into(),
                    sources.debug_zones,
                )
            } else {
                Row::new(
                    Self::ZONE_OUTLINES,
                    "Zone outlines",
                    Control::Toggle(outlines),
                )
                .detail("Tint every zone's bounds")
            }
            .keywords(&["debug", "zones", "debug-zones", "geometry", "bounds"]);

            let fps = self.control.live.fps();
            let fps_row = if sources.fps.is_pinned() {
                Self::pinned(Self::FPS, "Frame rate cap", fps.to_string(), sources.fps)
            } else {
                let options = self.fps_options();
                Row::new(
                    Self::FPS,
                    "Frame rate cap",
                    Control::Choice {
                        selected: options.iter().position(|f| *f == fps).unwrap_or(0),
                        options: options.iter().map(u32::to_string).collect(),
                    },
                )
                .detail(sources.fps.describe())
            }
            .keywords(&["fps", "frames", "refresh", "rate", "performance"]);

            vec![log_row, outlines_row, fps_row]
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let sources = self.control.sources();
            match (row, edit) {
                (Self::LOG_LEVEL, Edit::Choose(i)) if !sources.log_level.is_pinned() => {
                    let level = LOG_LEVELS.get(i).ok_or("No such level")?;
                    (self.control.log_level.set)(level)?;
                    tracing::info!(level, "control panel: log level changed");
                    self.saved(
                        self.control
                            .save(RuntimeUpdate::LogLevel(Some((*level).to_owned())), |s| {
                                &mut s.log_level
                            }),
                    )
                }
                (Self::ZONE_OUTLINES, Edit::Set(on)) if !sources.debug_zones.is_pinned() => {
                    self.control.live.set_zone_outlines(on);
                    self.saved(
                        self.control
                            .save(RuntimeUpdate::DebugZones(Some(on)), |s| &mut s.debug_zones),
                    )
                }
                (Self::FPS, Edit::Choose(i)) if !sources.fps.is_pinned() => {
                    let fps = *self.fps_options().get(i).ok_or("No such rate")?;
                    self.control.live.set_fps(fps);
                    self.saved(
                        self.control
                            .save(RuntimeUpdate::Fps(Some(fps)), |s| &mut s.fps),
                    )
                }
                (_, edit) => Err(format!("{row} does not support {edit:?}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::shell::control_panel::runtime::tests::fake;

    fn row<'a>(rows: &'a [Row], id: &str) -> &'a Row {
        rows.iter().find(|r| r.id == id).unwrap()
    }

    /// Log level and zone outlines apply now and are saved; a rate a variable
    /// pinned is read-only; a failed save keeps the change live and says so.
    #[tokio::test]
    async fn changes_apply_now_save_and_respect_pins() {
        let level = Arc::new(std::sync::Mutex::new("info".to_owned()));
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("c.toml");
        let mut control = fake(&config, Arc::default(), None);
        let (get, set) = (Arc::clone(&level), Arc::clone(&level));
        control.log_level = crate::shell::control_panel::runtime::LogLevel {
            get: Arc::new(move || Some(get.lock().unwrap().clone())),
            set: Arc::new(move |l| {
                *set.lock().unwrap() = l.to_owned();
                Ok(())
            }),
        };
        control.sources.lock().unwrap().fps = Source::Env("TZE_HUD_FPS");
        let section = DiagnosticsSection::new(control.clone());

        let rows = section.rows().await;
        assert_eq!(
            row(&rows, DiagnosticsSection::LOG_LEVEL).control,
            Control::Choice {
                options: LOG_LEVELS.iter().map(|l| (*l).to_owned()).collect(),
                selected: 2
            }
        );
        let fps = row(&rows, DiagnosticsSection::FPS);
        assert_eq!(fps.detail.as_deref(), Some("Set by TZE_HUD_FPS"));
        assert!(
            section
                .apply(DiagnosticsSection::FPS, Edit::Choose(0))
                .await
                .is_err()
        );
        assert_eq!(control.live.fps(), 60, "a pinned rate does not move");

        section
            .apply(DiagnosticsSection::LOG_LEVEL, Edit::Choose(3))
            .await
            .unwrap();
        assert_eq!(*level.lock().unwrap(), "debug");
        section
            .apply(DiagnosticsSection::ZONE_OUTLINES, Edit::Set(true))
            .await
            .unwrap();
        assert!(control.live.zone_outlines());
        let saved = std::fs::read_to_string(&config).unwrap();
        assert!(saved.contains("log_level = \"debug\"") && saved.contains("debug_zones = true"));
        assert_eq!(
            row(&section.rows().await, DiagnosticsSection::LOG_LEVEL)
                .detail
                .as_deref(),
            Some("Saved to config")
        );

        // The config became unwritable: the outline still turns off now.
        std::fs::remove_file(&config).unwrap();
        let err = section
            .apply(DiagnosticsSection::ZONE_OUTLINES, Edit::Set(false))
            .await
            .unwrap_err();
        assert!(
            err.starts_with("Applied until restart; not saved:"),
            "{err}"
        );
        assert!(!control.live.zone_outlines());
    }
}
