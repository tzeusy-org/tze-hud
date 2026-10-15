//! The extension seam: what a panel section offers and how edits reach it.
//!
//! A [`Section`] reads live facts into [`Row`]s and applies [`Edit`]s to them.
//! The panel owns everything else: layout, search, keyboard and pointer
//! handling, confirmation, error display and quick-action tiles. A new
//! capability (a theme, a widget, a runtime setting) is therefore a row in an
//! existing section or a new section; it never touches panel code.

use std::future::Future;
use std::pin::Pin;

use tze_hud_compositor::PanelBadge;

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
}

/// How a row's value is shown and changed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Control {
    /// A fact or a value set elsewhere; the panel cannot change it here.
    ReadOnly { text: String, mono: bool },
    /// On/off: Enter or click flips it, Left/Right set off/on.
    Toggle(bool),
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "first rows: connect, clear, unpair, hud-vniri.6")
    )]
    Action { confirm: Option<String> },
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
/// it reads current facts and stays cheap; `apply` performs the edit and
/// returns a message the row shows when it fails. Both run on the panel task
/// on the network runtime, never on the winit loop or an agent's task.
pub(crate) trait Section: Send + Sync {
    /// Stable id; prefixes every row id.
    fn id(&self) -> &'static str;
    fn title(&self) -> &'static str;
    fn rows(&self) -> BoxFuture<'_, Vec<Row>>;
    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>>;
}
