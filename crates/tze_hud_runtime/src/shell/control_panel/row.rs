//! The extension seam: what a panel section offers and how edits reach it.
//!
//! A [`Section`] reads live facts into [`Row`]s and applies [`Edit`]s to them.
//! The panel owns everything else: layout, search, keyboard and pointer
//! handling, confirmation, error display and quick-action tiles. A new
//! capability (a theme, a widget, a runtime setting) is therefore a row in an
//! existing section or a new section; it never touches panel code.

use std::future::Future;
use std::pin::Pin;

use tze_hud_compositor::{PanelBadge, PanelTone};

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One setting or action, as a section reports it right now.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    /// Unique across the panel: `<section id>.<name>`. Quick actions, focus
    /// and edits address rows by it.
    pub id: String,
    pub label: String,
    /// One short line under the label: state, effect, or where a value comes
    /// from.
    pub detail: Option<String>,
    /// Extra search terms (synonyms, config keys) beyond label, detail and
    /// section title.
    pub keywords: &'static [&'static str],
    pub control: Control,
    pub badge: Option<PanelBadge>,
    /// A suppressing state is on (agents paused, content hidden): drawn in the
    /// caution palette, also on its quick tile.
    pub caution: bool,
    /// A status dot before the label (e.g. an agent's connection).
    pub status: Option<PanelTone>,
}

impl Row {
    pub(crate) fn new(id: impl Into<String>, label: impl Into<String>, control: Control) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            detail: None,
            keywords: &[],
            control,
            badge: None,
            caution: false,
            status: None,
        }
    }

    pub(crate) fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub(crate) fn keywords(mut self, keywords: &'static [&'static str]) -> Self {
        self.keywords = keywords;
        self
    }

    pub(crate) fn badge(mut self, badge: Option<PanelBadge>) -> Self {
        self.badge = badge;
        self
    }

    pub(crate) fn caution(mut self, caution: bool) -> Self {
        self.caution = caution;
        self
    }

    pub(crate) fn status(mut self, status: PanelTone) -> Self {
        self.status = Some(status);
        self
    }
}

/// How a row's value is shown and changed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Control {
    /// A fact or a value set elsewhere; the panel cannot change it here.
    ReadOnly { text: String, mono: bool },
    /// On/off: Enter or click flips it, Left/Right set off/on.
    Toggle(bool),
    /// A toggle that widens trust when switched on: on asks `confirm` once
    /// first; off applies at once.
    Grant { on: bool, confirm: String },
    /// Shows or hides the rows under this one (`text` says what is there).
    /// Enter or click flips it, Left/Right close/open.
    Disclosure { open: bool, text: String },
    /// One of `options`: Left/Right or click cycles.
    Choice {
        options: Vec<String>,
        selected: usize,
    },
    /// A number shown as `text`: Left/Right or −/+ step it.
    Stepper { text: String },
    /// A button. `confirm` asks once before acting (destructive or
    /// trust-widening actions).
    Action { confirm: Option<String> },
    /// Text the owner types while the row has focus: the first keystroke
    /// replaces `text`, Enter submits it as [`Edit::Text`], Esc cancels.
    /// (On Connect an agent, Enter on the name copies the prompt.) With
    /// `confirm`, a changed value asks it once first: the first Enter shows
    /// the question, the second saves.
    Field {
        text: String,
        confirm: Option<String>,
    },
}

impl Control {
    /// The value this row shows, for the action log.
    pub(crate) fn value_text(&self) -> String {
        match self {
            Self::ReadOnly { text, .. } | Self::Stepper { text } | Self::Field { text, .. } => {
                text.clone()
            }
            Self::Toggle(on) | Self::Grant { on, .. } => on_off(*on).to_owned(),
            Self::Disclosure { open, .. } => if *open { "open" } else { "closed" }.to_owned(),
            Self::Choice { options, selected } => {
                options.get(*selected).cloned().unwrap_or_default()
            }
            Self::Action { .. } => String::new(),
        }
    }

    /// What `edit` asks this row to become, for the action log: the new
    /// value where the edit names one, else the step or the action.
    pub(crate) fn edited_text(&self, edit: &Edit) -> String {
        match (self, edit) {
            (_, Edit::Activate) => "activated".to_owned(),
            (_, Edit::Set(on)) => on_off(*on).to_owned(),
            (Self::Choice { options, .. }, Edit::Choose(n)) => options
                .get(*n)
                .cloned()
                .unwrap_or_else(|| format!("option {n}")),
            (_, Edit::Choose(n)) => format!("option {n}"),
            (_, Edit::Step(n)) => format!("{n:+} step"),
            (_, Edit::Text(text)) => text.clone(),
        }
    }

    /// The question asked before `edit` applies, if it needs one.
    pub(crate) fn confirm_for(&self, edit: &Edit) -> Option<&str> {
        match (self, edit) {
            (Control::Action { confirm }, Edit::Activate) => confirm.as_deref(),
            // Only granting asks, and only when it changes something.
            (Control::Grant { on: false, confirm }, Edit::Set(true)) => Some(confirm),
            // A field asks only when the typed value differs.
            (
                Control::Field {
                    text,
                    confirm: Some(confirm),
                },
                Edit::Text(typed),
            ) if typed.trim() != text.trim() => Some(confirm),
            _ => None,
        }
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// A change the panel asks a section to make to one of its rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Edit {
    /// Run an action (after any confirmation).
    Activate,
    /// Set a toggle.
    Set(bool),
    /// Select option `n` of a choice.
    Choose(usize),
    /// Step a stepper by `n` (negative: down).
    Step(i32),
    /// The text typed into a field, submitted with Enter.
    Text(String),
}

/// One card of the panel: a group of rows over one area of the HUD.
///
/// `rows` runs whenever the panel rebuilds (each interaction or refresh), so
/// it stays cheap (costly outside state is cached by `refresh`); `apply` performs the edit and
/// returns a message the row shows when it fails. Both run on the panel task
/// on the network runtime, never on the winit loop or an agent's task.
pub(crate) trait Section: Send + Sync {
    /// Stable id; prefixes every row id.
    fn id(&self) -> &'static str;
    fn title(&self) -> &'static str;
    /// The rows, from state the section holds or reads cheaply: called on
    /// every input while the panel is open.
    fn rows(&self) -> BoxFuture<'_, Vec<Row>>;
    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>>;

    /// A short line to keep on screen while the panel is closed, for state the
    /// owner set here that changes what the HUD shows (e.g. "Widgets
    /// hidden"). The panel adds the chord that reopens it. Read when the panel closes and on
    /// each chord while closed, so it suits state only the panel changes; a
    /// notice that tracks outside state needs a way to wake the panel task.
    fn closed_notice(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async { None })
    }

    /// A few words for the panel header's state line ("3 agents · 2 portals",
    /// "restart pending"), from state the section already holds; read on
    /// every rebuild, so it may take a short lock but must not await or do
    /// I/O.
    fn summary(&self) -> Option<String> {
        None
    }

    /// The panel opened: forget anything [`Self::refresh`] throttles, so the
    /// first refresh reads fresh state.
    fn opened(&self) {}

    /// Re-read outside state that is costly to read (locks other services
    /// take), for [`Self::rows`] to show. Called when the panel opens and
    /// once a second while it is open.
    fn refresh(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The action log names each row's value before and what the edit set.
    #[test]
    fn log_values_name_the_old_and_new_value() {
        let choice = Control::Choice {
            options: vec!["classic".into(), "tonal-glass".into()],
            selected: 0,
        };
        assert_eq!(choice.value_text(), "classic");
        assert_eq!(choice.edited_text(&Edit::Choose(1)), "tonal-glass");
        assert_eq!(Control::Toggle(true).value_text(), "on");
        assert_eq!(Control::Toggle(true).edited_text(&Edit::Set(false)), "off");
        let zoom = Control::Stepper {
            text: "100%".into(),
        };
        assert_eq!(zoom.value_text(), "100%");
        assert_eq!(zoom.edited_text(&Edit::Step(-1)), "-1 step");
        let port = Control::Field {
            text: "9090".into(),
            confirm: Some("Change it?".into()),
        };
        assert_eq!(
            port.confirm_for(&Edit::Text("9191".into())),
            Some("Change it?")
        );
        assert_eq!(
            port.confirm_for(&Edit::Text(" 9090".into())),
            None,
            "the same value saves without asking"
        );
        assert_eq!(port.edited_text(&Edit::Text("9191".into())), "9191");
        let restart = Control::Action { confirm: None };
        assert_eq!(restart.value_text(), "");
        assert_eq!(restart.edited_text(&Edit::Activate), "activated");
    }
}
