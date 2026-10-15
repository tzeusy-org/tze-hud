//! Input: the HUD's global chords and whether Windows registered them (the
//! same facts as `/admin/status`). Read-only until live rebinding lands.

use tze_hud_compositor::{PanelBadge, PanelTone};
use tze_hud_scene::config::{ChordRole, HudChords};

use super::super::row::{BoxFuture, Control, Edit, Row, Section};
use crate::operator::status::{HotkeyStatus, hotkey};

pub(crate) struct InputSection {
    chords: HudChords,
    /// Pairing is available (the MCP port is on); without it the pairing
    /// chord is not registered and not listed.
    pairing: bool,
}

impl InputSection {
    pub(crate) fn new(chords: HudChords, pairing: bool) -> Self {
        Self { chords, pairing }
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
                            "Shows a one-time pairing code",
                            &["hotkey", "shortcut", "pairing_hotkey", "pair", "connect"],
                        ),
                    };
                    let status = hotkey(role);
                    let detail = match &status {
                        HotkeyStatus::Failed { reason, .. } => format!("{does} · {reason}"),
                        _ => does.to_owned(),
                    };
                    Row::new(
                        id,
                        label,
                        Control::ReadOnly {
                            text: self.chords.get(role).to_string(),
                            mono: true,
                        },
                    )
                    .detail(detail)
                    .keywords(keywords)
                    .badge(registration_badge(&status))
                })
                .collect()
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { Err(format!("{row} is read-only ({edit:?})")) })
    }
}
