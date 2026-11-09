//! Input: the HUD's global chords and whether Windows registered them (the
//! same facts as `/admin/status`). A chord is typed in place: Windows takes
//! the new one before letting the old one go, so a refused chord leaves the
//! old one working; it is then saved to `[runtime]`.

use tze_hud_compositor::{PanelBadge, PanelTone};
use tze_hud_config::RuntimeUpdate;
use tze_hud_scene::config::{ChordRole, Hotkey, HudChords};

use super::super::chords::ChordControl;
use super::super::row::{BoxFuture, Control, Edit, Row, Section};
use super::super::runtime::ConfigWriter;
use crate::operator::status::{HotkeyStatus, hotkey};

pub(crate) struct InputSection {
    chords: ChordControl,
    /// Pairing is available (the MCP port is on); without it the pairing
    /// chord is not registered and not listed.
    pairing: bool,
    /// Saves a changed chord; `None`: no config file, it lasts the session.
    save: Option<ConfigWriter>,
}

impl InputSection {
    pub(crate) fn new(chords: ChordControl, pairing: bool, save: Option<ConfigWriter>) -> Self {
        Self {
            chords,
            pairing,
            save,
        }
    }

    fn role_of(row: &str) -> Option<ChordRole> {
        Some(match row {
            "input.pause_chord" => ChordRole::Pause,
            "input.panel_chord" => ChordRole::Panel,
            "input.pairing_chord" => ChordRole::Pairing,
            "input.answer_chord" => ChordRole::Answer,
            _ => return None,
        })
    }
}

fn registration_badge(status: &HotkeyStatus) -> Option<PanelBadge> {
    let (text, tone) = match status {
        HotkeyStatus::NotApplicable => return None,
        HotkeyStatus::Registered { .. } => ("registered", PanelTone::Success),
        HotkeyStatus::Pending { .. } => ("pending", PanelTone::Caution),
        HotkeyStatus::Failed { .. } => ("not registered", PanelTone::Danger),
    };
    Some(PanelBadge {
        text: text.into(),
        tone,
    })
}

impl Section for InputSection {
    fn id(&self) -> &'static str {
        "input"
    }

    fn title(&self) -> &'static str {
        "Input"
    }

    fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
        Box::pin(async move {
            ChordRole::ALL
                .into_iter()
                .filter(|role| self.pairing || *role != ChordRole::Pairing)
                .map(|role| {
                    let (id, label, does, keywords): (_, _, _, &'static [&'static str]) = match role
                    {
                        ChordRole::Pause => (
                            "input.pause_chord",
                            "Pause chord",
                            "Pauses agents and opens this panel",
                            &["hotkey", "shortcut", "safe_mode_hotkey", "safe mode"],
                        ),
                        ChordRole::Panel => (
                            "input.panel_chord",
                            "Panel chord",
                            "Opens this panel; agents keep running",
                            &["hotkey", "shortcut", "panel_hotkey", "settings"],
                        ),
                        ChordRole::Pairing => (
                            "input.pairing_chord",
                            "Pairing chord",
                            "Opens Connect an agent with a one-time code",
                            &["hotkey", "shortcut", "pairing_hotkey", "pair", "connect"],
                        ),
                        ChordRole::Answer => (
                            "input.answer_chord",
                            "Answer chord",
                            "Focuses the newest question; 1–4 or Enter answers, Esc returns",
                            &[
                                "hotkey",
                                "shortcut",
                                "answer_hotkey",
                                "question",
                                "notification",
                            ],
                        ),
                    };
                    let status = hotkey(role);
                    let chord = self.chords.get().get(role);
                    let changed = chord != HudChords::default().get(role);
                    // Inferred, not read from the file: a changed chord with
                    // a writer was saved (a failed save says so on the row).
                    let source = match (changed, &self.save) {
                        (false, _) => "Default",
                        (true, Some(_)) => "Saved to config",
                        (true, None) => "Lasts until restart",
                    };
                    let detail = match &status {
                        HotkeyStatus::Failed { reason, .. } => {
                            format!("{does} · {source} · {reason}")
                        }
                        _ => format!("{does} · {source}"),
                    };
                    let text = chord.to_string();
                    let control = if self.chords.rebind.is_some() {
                        Control::Field {
                            text,
                            confirm: None,
                        }
                    } else {
                        Control::ReadOnly { text, mono: true }
                    };
                    Row::new(id, label, control)
                        .detail(detail)
                        .keywords(keywords)
                        .badge(registration_badge(&status))
                        .resettable(changed && self.chords.rebind.is_some())
                })
                .collect()
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let Some(role) = Self::role_of(row) else {
                return Err(format!("{row} does not support {edit:?}"));
            };
            let new = match &edit {
                Edit::Text(text) => Hotkey::parse(text.trim())
                    .map_err(|e| format!("Type a chord like Ctrl+Shift+F8: {e}"))?,
                Edit::Reset => HudChords::default().get(role),
                // Enter without typing: nothing to change.
                Edit::Activate => return Ok(()),
                _ => return Err(format!("{row} does not support {edit:?}")),
            };
            // A reset clears the config key, so the default follows releases.
            let saved = (edit != Edit::Reset).then(|| new.to_string());
            let chords = self.chords.clone();
            tokio::task::spawn_blocking(move || chords.rebind(role, new))
                .await
                .map_err(|e| e.to_string())??;
            tracing::info!(?role, chord = %new, "control panel: chord changed");
            let Some(save) = &self.save else {
                return Ok(());
            };
            save(&[RuntimeUpdate::Hotkey(role, saved)])
                .map_err(|e| format!("Works until restart; not saved: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Chord rows name their source; a changed chord offers Reset, which
    /// rebinds the default and clears the config key.
    #[tokio::test]
    async fn chord_rows_name_their_source_and_reset_to_the_default() {
        let saved: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = Arc::clone(&saved);
        let save: ConfigWriter = Arc::new(move |updates: &[RuntimeUpdate]| {
            log.lock().unwrap().push(format!("{updates:?}"));
            Ok(())
        });
        let chords = ChordControl::new(HudChords::default(), Some(Arc::new(|_, _| Ok(()))));
        let section = InputSection::new(chords.clone(), false, Some(save));
        for (role, id) in [
            (ChordRole::Panel, "input.panel_chord"),
            (ChordRole::Answer, "input.answer_chord"),
        ] {
            let row_for = |rows: Vec<Row>| rows.into_iter().find(|r| r.id == id).unwrap();
            let row = row_for(section.rows().await);
            assert!(row.detail.unwrap().ends_with("· Default"));
            assert!(!row.reset);

            section
                .apply(id, Edit::Text("Ctrl+Shift+F8".into()))
                .await
                .unwrap();
            let row = row_for(section.rows().await);
            assert!(row.detail.unwrap().ends_with("· Saved to config"));
            assert!(row.reset);
            section.apply(id, Edit::Reset).await.unwrap();
            assert_eq!(chords.get().get(role), HudChords::default().get(role));
            assert_eq!(
                saved.lock().unwrap().last().unwrap(),
                &format!("{:?}", [RuntimeUpdate::Hotkey(role, None)])
            );
            assert!(!row_for(section.rows().await).reset);

            // Without a config file a change lasts the session.
            let unsaved = InputSection::new(chords.clone(), false, None);
            unsaved
                .apply(id, Edit::Text("Ctrl+Shift+F8".into()))
                .await
                .unwrap();
            let row = row_for(unsaved.rows().await);
            assert!(row.detail.unwrap().ends_with("· Lasts until restart"));
            unsaved.apply(id, Edit::Reset).await.unwrap();
        }
        assert!(
            !section
                .rows()
                .await
                .iter()
                .any(|r| r.id == "input.pairing_chord")
        );
    }
}
