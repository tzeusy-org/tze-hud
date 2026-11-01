//! Input: the HUD's global chords and whether Windows registered them (the
//! same facts as `/admin/status`). A chord is typed in place: Windows takes
//! the new one before letting the old one go, so a refused chord leaves the
//! old one working; it is then saved to `[runtime]`.

use tze_hud_compositor::{PanelBadge, PanelTone};
use tze_hud_config::RuntimeUpdate;
use tze_hud_scene::config::{ChordRole, Hotkey};

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
                    };
                    let status = hotkey(role);
                    let detail = match &status {
                        HotkeyStatus::Failed { reason, .. } => format!("{does} · {reason}"),
                        _ => does.to_owned(),
                    };
                    let text = self.chords.get().get(role).to_string();
                    let control = if self.chords.rebind.is_some() {
                        Control::Field { text }
                    } else {
                        Control::ReadOnly { text, mono: true }
                    };
                    Row::new(id, label, control)
                        .detail(detail)
                        .keywords(keywords)
                        .badge(registration_badge(&status))
                })
                .collect()
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let (Some(role), Edit::Text(text)) = (Self::role_of(row), &edit) else {
                return match edit {
                    // Enter without typing: nothing to change.
                    Edit::Activate => Ok(()),
                    _ => Err(format!("{row} does not support {edit:?}")),
                };
            };
            let new = Hotkey::parse(text.trim())
                .map_err(|e| format!("Type a chord like Ctrl+Shift+F9: {e}"))?;
            let chords = self.chords.clone();
            tokio::task::spawn_blocking(move || chords.rebind(role, new))
                .await
                .map_err(|e| e.to_string())??;
            tracing::info!(?role, chord = %new, "control panel: chord changed");
            let Some(save) = &self.save else {
                return Ok(());
            };
            save(&[RuntimeUpdate::Hotkey(role, Some(new.to_string()))])
                .map_err(|e| format!("Works until restart; not saved: {e}"))
        })
    }
}
