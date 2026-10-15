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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "first rows: theme and font, hud-vniri.5")
    )]
    Choice {
        options: Vec<String>,
        selected: usize,
    },
    /// A number shown as `text`: Left/Right or −/+ step it.
    #[cfg_attr(not(test), expect(dead_code, reason = "first row: zoom, hud-vniri.5"))]
    Stepper { text: String },
    /// A button. `confirm` asks once before acting (destructive or
    /// trust-widening actions).
    Action { confirm: Option<String> },
}

impl Control {
    /// The question asked before `edit` applies, if it needs one.
    pub(crate) fn confirm_for(&self, edit: Edit) -> Option<&str> {
        match (self, edit) {
            (Control::Action { confirm }, Edit::Activate) => confirm.as_deref(),
            // Only granting asks, and only when it changes something.
            (Control::Grant { on: false, confirm }, Edit::Set(true)) => Some(confirm),
            _ => None,
        }
    }
}

/// A change the panel asks a section to make to one of its rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Edit {
    /// Run an action (after any confirmation).
    Activate,
    /// Set a toggle.
    Set(bool),
    /// Select option `n` of a choice.
    Choose(usize),
    /// Step a stepper by `n` (negative: down).
    Step(i32),
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

    /// Re-read outside state that is costly to read (locks other services
    /// take), for [`Self::rows`] to show. Called when the panel opens and
    /// once a second while it is open.
    fn refresh(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
