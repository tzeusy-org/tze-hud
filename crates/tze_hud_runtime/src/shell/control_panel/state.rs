//! The panel's pure core: the open/pause state machine, the view model built
//! from section rows, and keyboard/pointer handling. No I/O and no clock, so
//! every behaviour here is unit-tested directly; the task in `mod.rs` only
//! gathers rows, runs the resulting [`Effect`]s and publishes the model.

use tze_hud_compositor::{
    ControlPanelGeometry, ControlPanelModel, PanelBadge, PanelBody, PanelChip, PanelControl,
    PanelFocus, PanelHitTarget, PanelItem, PanelPart, PanelSection, PanelTone,
};
use tze_hud_scene::config::ChordRole;

use super::row::{Control, Edit, Row};
use super::search::{Haystack, score};

// ─── Chords ──────────────────────────────────────────────────────────────────

/// What the panel does when it opens or closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelChange {
    Open(ChordRole),
    Close,
    Keep,
}

/// The decision for one chord press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChordOutcome {
    /// Pause (`Some(true)`) or resume (`Some(false)`) agents.
    pub pause: Option<bool>,
    pub panel: PanelChange,
    /// Show the pairing card (the pairing chord).
    pub pairing: bool,
}

/// The chord state machine (spec CP-1):
///
/// ```text
/// Closed --pause chord--> Open(paused, opened_by=Pause)
/// Closed --panel chord--> Open(running, opened_by=Panel)
/// Open   --pause chord--> toggle pause; if opened_by=Pause and now running: close
/// Open   --panel chord--> close, resuming agents if paused
/// any    --pairing chord--> close (resuming) and show the pairing card
/// ```
///
/// Closing always resumes, so "paused with the panel closed" cannot happen.
pub(crate) fn on_chord(
    opened_by: Option<ChordRole>,
    paused: bool,
    chord: ChordRole,
) -> ChordOutcome {
    let resume_if_paused = paused.then_some(false);
    match (opened_by, chord) {
        (None, ChordRole::Pause) => ChordOutcome {
            pause: (!paused).then_some(true),
            panel: PanelChange::Open(ChordRole::Pause),
            pairing: false,
        },
        (None, ChordRole::Panel) => ChordOutcome {
            pause: None,
            panel: PanelChange::Open(ChordRole::Panel),
            pairing: false,
        },
        (Some(by), ChordRole::Pause) => {
            let now_paused = !paused;
            let close = by == ChordRole::Pause && !now_paused;
            ChordOutcome {
                pause: Some(now_paused),
                panel: if close {
                    PanelChange::Close
                } else {
                    PanelChange::Keep
                },
                pairing: false,
            }
        }
        (Some(_), ChordRole::Panel) => ChordOutcome {
            pause: resume_if_paused,
            panel: PanelChange::Close,
            pairing: false,
        },
        (opened_by, ChordRole::Pairing) => ChordOutcome {
            pause: opened_by.and(resume_if_paused),
            panel: if opened_by.is_some() {
                PanelChange::Close
            } else {
                PanelChange::Keep
            },
            pairing: true,
        },
    }
}

// ─── Quick actions ───────────────────────────────────────────────────────────

/// How a quick action is reached from the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Accelerator {
    /// Alt+letter while the panel is open.
    Alt(char),
    /// The global pause chord.
    PauseChord,
    /// Ctrl+− / Ctrl+= step the row.
    CtrlMinusEquals,
}

/// A row promoted to a large header tile. A tile shows only while its row
/// exists, so a quick action never points at nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QuickAction {
    pub row: &'static str,
    pub accelerator: Accelerator,
}

/// The fixed quick-action set, left to right (spec CP-12). Rows that no
/// section offers yet are skipped.
pub(crate) const QUICK_ACTIONS: &[QuickAction] = &[
    QuickAction {
        row: "visibility.hide_all",
        accelerator: Accelerator::Alt('h'),
    },
    QuickAction {
        row: "agents.pause",
        accelerator: Accelerator::PauseChord,
    },
    QuickAction {
        row: "appearance.zoom",
        accelerator: Accelerator::CtrlMinusEquals,
    },
    QuickAction {
        row: "appearance.theme",
        accelerator: Accelerator::Alt('t'),
    },
    QuickAction {
        row: "agents.connect",
        accelerator: Accelerator::Alt('c'),
    },
];

// ─── Open state ──────────────────────────────────────────────────────────────

/// Everything the panel remembers while open. Gone when it closes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Open {
    pub opened_by: ChordRole,
    pub query: String,
    pub focus: PanelFocus,
    pub hover: Option<PanelHitTarget>,
    pub scroll: f32,
    /// The action row waiting for a second Enter/click to confirm.
    pub confirming: Option<String>,
    /// The last failed edit: (row id, message), shown on that row.
    pub error: Option<(String, String)>,
    /// Windows refused (or the owner moved) keyboard focus.
    pub focus_refused: bool,
    /// The query just changed: focus the best match on the next build.
    pub focus_first_result: bool,
}

impl Open {
    pub(crate) fn new(opened_by: ChordRole) -> Self {
        Self {
            opened_by,
            query: String::new(),
            focus: PanelFocus::Search,
            hover: None,
            scroll: 0.0,
            confirming: None,
            error: None,
            focus_refused: false,
            focus_first_result: false,
        }
    }
}

// ─── View ────────────────────────────────────────────────────────────────────

/// One section's current rows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SectionRows {
    pub id: &'static str,
    pub title: &'static str,
    pub rows: Vec<Row>,
}

/// Facts the view shows that no section owns.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ViewContext {
    pub paused: bool,
    /// The pause chord as shown on its tile (e.g. `Ctrl+Shift+F12`).
    pub pause_chord: String,
}

/// The focus order the keyboard walks, from the last build.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Nav {
    /// Row id behind each tile.
    pub tiles: Vec<String>,
    /// Each tile's control (its row may be filtered out of `rows` by a
    /// search, but the tile still acts).
    pub tile_controls: Vec<Control>,
    /// Rows in reading order with their controls.
    pub rows: Vec<(String, Control)>,
    /// Index into `rows` where each listed section starts (sections view).
    pub sections: Vec<usize>,
}

impl Nav {
    fn row_index(&self, id: &str) -> Option<usize> {
        self.rows.iter().position(|(r, _)| r == id)
    }

    fn control(&self, id: &str) -> Option<&Control> {
        self.rows
            .iter()
            .find(|(r, _)| r == id)
            .map(|(_, c)| c)
            .or_else(|| {
                let i = self.tiles.iter().position(|t| t == id)?;
                self.tile_controls.get(i)
            })
    }
}

fn tile_shortcut(accelerator: Accelerator, ctx: &ViewContext) -> String {
    match accelerator {
        Accelerator::Alt(c) => format!("Alt+{}", c.to_ascii_uppercase()),
        Accelerator::PauseChord => ctx.pause_chord.clone(),
        Accelerator::CtrlMinusEquals => "Ctrl+− / Ctrl+=".to_owned(),
    }
}

fn item_for(row: &Row, open: &Open, context: Option<&str>) -> PanelItem {
    let control = match &row.control {
        Control::ReadOnly { text, mono } => PanelControl::ReadOnly {
            text: text.clone(),
            mono: *mono,
        },
        Control::Toggle(on) => PanelControl::Toggle(*on),
        Control::Choice { options, selected } => {
            PanelControl::Choice(options.get(*selected).cloned().unwrap_or_default())
        }
        Control::Stepper { text } => PanelControl::Stepper(text.clone()),
        Control::Action { .. } => PanelControl::Action,
    };
    let mut item = PanelItem {
        id: row.id.clone(),
        label: row.label.clone(),
        detail: row.detail.clone(),
        detail_tone: PanelTone::Normal,
        context: context.map(str::to_owned),
        control,
        badge: row.badge.clone(),
        shortcut: None,
        emphasis: if row.caution {
            PanelTone::Caution
        } else {
            PanelTone::Normal
        },
    };
    if let Some((id, message)) = &open.error
        && *id == row.id
    {
        item.detail = Some(message.clone());
        item.detail_tone = PanelTone::Danger;
    }
    if open.confirming.as_deref() == Some(&row.id)
        && let Control::Action {
            confirm: Some(question),
        } = &row.control
    {
        item.detail = Some(question.clone());
        item.detail_tone = PanelTone::Danger;
        item.emphasis = PanelTone::Danger;
        item.badge = Some(PanelBadge {
            text: "Enter to confirm · Esc to cancel".into(),
            tone: PanelTone::Danger,
        });
    }
    item
}

/// Rows matching `query`, best first (ties keep panel order).
fn search_results<'a>(query: &str, sections: &'a [SectionRows]) -> Vec<(&'a Row, &'a str)> {
    let mut scored: Vec<(u32, &Row, &str)> = sections
        .iter()
        .flat_map(|s| s.rows.iter().map(move |r| (r, s.title)))
        .filter_map(|(row, title)| score(query, &Haystack::of(row, title)).map(|n| (n, row, title)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored
        .into_iter()
        .map(|(_, row, title)| (row, title))
        .collect()
}

/// Build the model the compositor draws, and the focus order, from `open`
/// and the sections' current rows. Repairs focus when its row went away.
pub(crate) fn build(
    open: &mut Open,
    sections: &[SectionRows],
    ctx: &ViewContext,
) -> (ControlPanelModel, Nav) {
    let find = |id: &str| sections.iter().flat_map(|s| &s.rows).find(|r| r.id == id);
    let mut nav = Nav::default();

    let mut tiles = Vec::new();
    for quick in QUICK_ACTIONS {
        let Some(row) = find(quick.row) else { continue };
        let mut item = item_for(row, open, None);
        item.shortcut = Some(tile_shortcut(quick.accelerator, ctx));
        if matches!(quick.accelerator, Accelerator::CtrlMinusEquals) {
            // Stepper tiles show the keys under the label; the right side
            // holds the − value + buttons.
            item.detail = item.shortcut.take();
        }
        nav.tiles.push(row.id.clone());
        nav.tile_controls.push(row.control.clone());
        tiles.push(item);
    }

    let listed: Vec<&SectionRows> = sections.iter().filter(|s| !s.rows.is_empty()).collect();
    let chips = listed
        .iter()
        .enumerate()
        .filter(|(i, _)| *i < 9)
        .map(|(i, s)| PanelChip {
            title: s.title.to_owned(),
            key: (i + 1).to_string(),
        })
        .collect();

    let query = open.query.trim();
    let body = if query.is_empty() {
        let mut cards = Vec::new();
        for (i, section) in listed.iter().enumerate() {
            nav.sections.push(nav.rows.len());
            cards.push(PanelSection {
                id: section.id.to_owned(),
                title: section.title.to_owned(),
                key: (i < 9).then(|| format!("Alt+{}", i + 1)),
                items: section
                    .rows
                    .iter()
                    .map(|r| item_for(r, open, None))
                    .collect(),
            });
            nav.rows.extend(
                section
                    .rows
                    .iter()
                    .map(|r| (r.id.clone(), r.control.clone())),
            );
        }
        PanelBody::Sections(cards)
    } else {
        let results = search_results(query, sections);
        if results.is_empty() {
            PanelBody::Empty(format!(
                "No settings match “{query}”. Esc clears the search."
            ))
        } else {
            nav.sections.push(0);
            nav.rows = results
                .iter()
                .map(|(r, _)| (r.id.clone(), r.control.clone()))
                .collect();
            let n = results.len();
            PanelBody::Results {
                title: format!(
                    "{n} {} for “{query}”",
                    if n == 1 { "result" } else { "results" }
                ),
                items: results
                    .iter()
                    .map(|(row, title)| item_for(row, open, Some(title)))
                    .collect(),
            }
        }
    };

    // Focus repair and query refocus.
    if std::mem::take(&mut open.focus_first_result) {
        open.focus = if query.is_empty() {
            PanelFocus::Search
        } else {
            nav.rows
                .first()
                .map_or(PanelFocus::Search, |(id, _)| PanelFocus::Row(id.clone()))
        };
    }
    match &open.focus {
        PanelFocus::Row(id) if nav.row_index(id).is_none() => open.focus = PanelFocus::Search,
        PanelFocus::Tile(i) if *i >= nav.tiles.len() => open.focus = PanelFocus::Search,
        _ => {}
    }
    if open
        .confirming
        .as_ref()
        .is_some_and(|id| open.focus != PanelFocus::Row(id.clone()) && !nav.tiles.contains(id))
    {
        open.confirming = None;
    }

    let mut hints: Vec<String> = [
        "Type to search",
        "↑↓ move",
        "Tab next section",
        "←→ change",
        "Enter act",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if !nav.sections.is_empty() && query.is_empty() {
        hints.push("Alt+1–9 jump".into());
    }
    let hint_end = if !query.is_empty() {
        "Esc clear search"
    } else if ctx.paused {
        "Esc close and resume"
    } else {
        "Esc close"
    };
    let model = ControlPanelModel {
        query: open.query.clone(),
        placeholder: "Search every HUD setting".into(),
        notice: open
            .focus_refused
            .then(|| "Click the panel to type · chords still work".to_owned()),
        tiles,
        chips,
        state_line: String::new(),
        body,
        hints,
        hint_end: hint_end.into(),
        focus: open.focus.clone(),
        hover: open.hover.clone(),
        scroll_px: open.scroll,
        scale: 1.0,
    };
    (model, nav)
}

// ─── Input ───────────────────────────────────────────────────────────────────

/// A key the panel handles while open (the winit loop translates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelKey {
    Up,
    Down,
    Left,
    Right,
    Tab,
    BackTab,
    Enter,
    Space,
    Escape,
    Backspace,
    Home,
    End,
    PageUp,
    PageDown,
    /// Alt+1–9: jump to that section.
    Section(u8),
    /// Alt+letter (lowercase): a quick action.
    Alt(char),
    ZoomIn,
    ZoomOut,
}

/// What an input asks the panel task to do besides updating `Open`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    Apply { row: String, edit: Edit },
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gesture {
    Activate,
    Left,
    Right,
}

fn gesture_for(part: PanelPart) -> Gesture {
    match part {
        PanelPart::Body => Gesture::Activate,
        PanelPart::Decrement => Gesture::Left,
        PanelPart::Increment => Gesture::Right,
    }
}

/// The edit a gesture makes to a control, if any.
fn edit_for(control: &Control, gesture: Gesture) -> Option<Edit> {
    match (control, gesture) {
        (Control::Toggle(on), Gesture::Activate) => Some(Edit::Set(!on)),
        (Control::Toggle(_), Gesture::Left) => Some(Edit::Set(false)),
        (Control::Toggle(_), Gesture::Right) => Some(Edit::Set(true)),
        (Control::Choice { options, selected }, gesture) if !options.is_empty() => {
            let n = options.len();
            Some(Edit::Choose(match gesture {
                Gesture::Left => (selected + n - 1) % n,
                Gesture::Activate | Gesture::Right => (selected + 1) % n,
            }))
        }
        (Control::Stepper { .. }, Gesture::Left) => Some(Edit::Step(-1)),
        (Control::Stepper { .. }, Gesture::Right) => Some(Edit::Step(1)),
        (Control::Action { .. }, Gesture::Activate) => Some(Edit::Activate),
        _ => None,
    }
}

/// Run `gesture` on row `id`, asking once first for a confirmed action.
fn act(open: &mut Open, nav: &Nav, id: &str, gesture: Gesture) -> Vec<Effect> {
    let Some(control) = nav.control(id) else {
        return Vec::new();
    };
    let Some(edit) = edit_for(control, gesture) else {
        return Vec::new();
    };
    if let Control::Action { confirm: Some(_) } = control
        && open.confirming.as_deref() != Some(id)
    {
        open.confirming = Some(id.to_owned());
        return Vec::new();
    }
    open.confirming = None;
    open.error = None;
    vec![Effect::Apply {
        row: id.to_owned(),
        edit,
    }]
}

fn focus_row(open: &mut Open, nav: &Nav, index: usize, geometry: Option<&ControlPanelGeometry>) {
    let Some((id, _)) = nav.rows.get(index) else {
        return;
    };
    open.focus = PanelFocus::Row(id.clone());
    if let Some(g) = geometry
        && let Some(span) = g.rows.iter().find(|s| s.id == *id)
    {
        open.scroll = g.reveal(span, open.scroll);
    }
}

fn jump_to_section(open: &mut Open, nav: &Nav, n: usize, geometry: Option<&ControlPanelGeometry>) {
    let Some(&start) = nav.sections.get(n) else {
        return;
    };
    focus_row(open, nav, start, geometry);
    if let Some(g) = geometry
        && let Some(span) = g.sections.get(n)
    {
        open.scroll = span.top.clamp(0.0, g.max_scroll);
    }
}

/// The section index the focused row is in.
fn current_section(nav: &Nav, index: usize) -> usize {
    nav.sections
        .iter()
        .rposition(|&start| start <= index)
        .unwrap_or(0)
}

/// Handle one key. Typing goes through [`on_text`]; chords through
/// [`on_chord`].
pub(crate) fn on_key(
    open: &mut Open,
    nav: &Nav,
    key: PanelKey,
    geometry: Option<&ControlPanelGeometry>,
) -> Vec<Effect> {
    let row_index = match &open.focus {
        PanelFocus::Row(id) => nav.row_index(id),
        _ => None,
    };
    if !matches!(key, PanelKey::Enter | PanelKey::Space | PanelKey::Escape) {
        open.confirming = None;
    }
    match key {
        PanelKey::Escape => {
            if !open.query.is_empty() {
                open.query.clear();
                open.focus = PanelFocus::Search;
                open.scroll = 0.0;
            } else if open.confirming.take().is_none() {
                return vec![Effect::Close];
            }
        }
        PanelKey::Down => match (&open.focus, row_index) {
            (PanelFocus::Search, _) if nav.tiles.is_empty() || !open.query.is_empty() => {
                focus_row(open, nav, 0, geometry)
            }
            (PanelFocus::Search, _) => open.focus = PanelFocus::Tile(0),
            (PanelFocus::Tile(_), _) => focus_row(open, nav, 0, geometry),
            (PanelFocus::Row(_), Some(i)) => focus_row(open, nav, i + 1, geometry),
            (PanelFocus::Row(_), None) => {}
        },
        PanelKey::Up => match (&open.focus, row_index) {
            (PanelFocus::Row(_), Some(0)) if nav.tiles.is_empty() || !open.query.is_empty() => {
                open.focus = PanelFocus::Search
            }
            (PanelFocus::Row(_), Some(0)) => open.focus = PanelFocus::Tile(0),
            (PanelFocus::Row(_), Some(i)) => focus_row(open, nav, i - 1, geometry),
            (PanelFocus::Tile(_), _) => open.focus = PanelFocus::Search,
            _ => {}
        },
        PanelKey::Left | PanelKey::Right => {
            let gesture = if key == PanelKey::Left {
                Gesture::Left
            } else {
                Gesture::Right
            };
            match open.focus.clone() {
                PanelFocus::Tile(i) => {
                    let id = nav.tiles[i].clone();
                    // Steppers step; elsewhere Left/Right move between tiles.
                    if matches!(nav.control(&id), Some(Control::Stepper { .. })) {
                        return act(open, nav, &id, gesture);
                    }
                    let last = nav.tiles.len().saturating_sub(1);
                    open.focus = PanelFocus::Tile(if gesture == Gesture::Left {
                        i.saturating_sub(1)
                    } else {
                        (i + 1).min(last)
                    });
                }
                PanelFocus::Row(id) => return act(open, nav, &id, gesture),
                PanelFocus::Search => {}
            }
        }
        PanelKey::Tab | PanelKey::BackTab => {
            // Groups: search, tiles, then each section.
            let forward = key == PanelKey::Tab;
            let group = match &open.focus {
                PanelFocus::Search => 0,
                PanelFocus::Tile(_) => 1,
                PanelFocus::Row(_) => 2 + row_index.map_or(0, |i| current_section(nav, i)),
            };
            let groups = 2 + nav.sections.len();
            let mut next = group;
            loop {
                next = if forward {
                    (next + 1) % groups
                } else {
                    (next + groups - 1) % groups
                };
                if next != 1 || !nav.tiles.is_empty() {
                    break;
                }
            }
            match next {
                0 => open.focus = PanelFocus::Search,
                1 => open.focus = PanelFocus::Tile(0),
                n => jump_to_section(open, nav, n - 2, geometry),
            }
        }
        PanelKey::Section(n) => {
            jump_to_section(open, nav, usize::from(n).saturating_sub(1), geometry)
        }
        PanelKey::Home => focus_row(open, nav, 0, geometry),
        PanelKey::End => focus_row(open, nav, nav.rows.len().saturating_sub(1), geometry),
        PanelKey::PageUp | PanelKey::PageDown => {
            let down = key == PanelKey::PageDown;
            let page = geometry.map_or(0.0, |g| g.viewport_height);
            let from = row_index.unwrap_or(0);
            let top_of = |i: usize| {
                geometry
                    .zip(nav.rows.get(i))
                    .and_then(|(g, (id, _))| g.rows.iter().find(|s| s.id == *id))
                    .map(|s| s.top)
            };
            let target = match top_of(from) {
                Some(start) if page > 0.0 => {
                    let goal = if down { start + page } else { start - page };
                    let candidates = 0..nav.rows.len();
                    if down {
                        candidates
                            .filter(|&i| top_of(i).is_some_and(|t| t >= goal))
                            .min_by(|a, b| {
                                top_of(*a)
                                    .unwrap_or(0.0)
                                    .total_cmp(&top_of(*b).unwrap_or(0.0))
                            })
                            .unwrap_or(nav.rows.len().saturating_sub(1))
                    } else {
                        candidates
                            .filter(|&i| top_of(i).is_some_and(|t| t <= goal))
                            .max_by(|a, b| {
                                top_of(*a)
                                    .unwrap_or(0.0)
                                    .total_cmp(&top_of(*b).unwrap_or(0.0))
                            })
                            .unwrap_or(0)
                    }
                }
                _ if down => (from + 8).min(nav.rows.len().saturating_sub(1)),
                _ => from.saturating_sub(8),
            };
            focus_row(open, nav, target, geometry);
        }
        PanelKey::Enter | PanelKey::Space => match open.focus.clone() {
            PanelFocus::Search if key == PanelKey::Space => on_text(open, " "),
            PanelFocus::Search => {
                if !open.query.is_empty()
                    && let Some((id, _)) = nav.rows.first()
                {
                    let id = id.clone();
                    open.focus = PanelFocus::Row(id.clone());
                    return act(open, nav, &id, Gesture::Activate);
                }
            }
            PanelFocus::Tile(i) => {
                let id = nav.tiles[i].clone();
                return act(open, nav, &id, Gesture::Activate);
            }
            PanelFocus::Row(id) => return act(open, nav, &id, Gesture::Activate),
        },
        PanelKey::Backspace => {
            if open.query.pop().is_some() {
                open.scroll = 0.0;
                open.focus_first_result = true;
            }
        }
        PanelKey::Alt(c) => {
            let quick = QUICK_ACTIONS
                .iter()
                .find(|q| q.accelerator == Accelerator::Alt(c));
            if let Some(quick) = quick
                && nav.control(quick.row).is_some()
            {
                if let Some(i) = nav.tiles.iter().position(|t| t == quick.row) {
                    open.focus = PanelFocus::Tile(i);
                }
                return act(open, nav, quick.row, Gesture::Activate);
            }
        }
        PanelKey::ZoomIn | PanelKey::ZoomOut => {
            let quick = QUICK_ACTIONS
                .iter()
                .find(|q| q.accelerator == Accelerator::CtrlMinusEquals);
            if let Some(quick) = quick {
                let gesture = if key == PanelKey::ZoomIn {
                    Gesture::Right
                } else {
                    Gesture::Left
                };
                return act(open, nav, quick.row, gesture);
            }
        }
    }
    Vec::new()
}

/// Typed text goes to the search query, wherever focus is (type-to-search).
pub(crate) fn on_text(open: &mut Open, text: &str) {
    let typed: String = text.chars().filter(|c| !c.is_control()).collect();
    if typed.is_empty() || (open.query.is_empty() && typed.trim().is_empty()) {
        return;
    }
    open.query.push_str(&typed);
    open.scroll = 0.0;
    open.confirming = None;
    open.focus_first_result = true;
}

/// A click (press and release on the same target).
pub(crate) fn on_click(
    open: &mut Open,
    nav: &Nav,
    target: &PanelHitTarget,
    geometry: Option<&ControlPanelGeometry>,
) -> Vec<Effect> {
    match target {
        PanelHitTarget::Search => {
            open.confirming = None;
            open.focus = PanelFocus::Search;
            Vec::new()
        }
        PanelHitTarget::Chip(i) => {
            open.confirming = None;
            jump_to_section(open, nav, *i, geometry);
            Vec::new()
        }
        PanelHitTarget::Tile(i, part) => {
            let Some(id) = nav.tiles.get(*i).cloned() else {
                return Vec::new();
            };
            if open.confirming.as_deref() != Some(id.as_str()) {
                open.confirming = None;
            }
            open.focus = PanelFocus::Tile(*i);
            act(open, nav, &id, gesture_for(*part))
        }
        PanelHitTarget::Row(id, part) => {
            if open.confirming.as_deref() != Some(id.as_str()) {
                open.confirming = None;
            }
            if let Some(i) = nav.row_index(id) {
                focus_row(open, nav, i, geometry);
            }
            act(open, nav, id, gesture_for(*part))
        }
        PanelHitTarget::Panel => Vec::new(),
    }
}

/// Wheel scrolling (`dy` > 0 scrolls the body down).
pub(crate) fn on_scroll(open: &mut Open, dy: f32, geometry: Option<&ControlPanelGeometry>) {
    let max = geometry.map_or(f32::MAX, |g| g.max_scroll);
    if dy.is_finite() {
        open.scroll = (open.scroll + dy).clamp(0.0, max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_compositor::PanelSpan;

    const PAUSE: ChordRole = ChordRole::Pause;
    const PANEL: ChordRole = ChordRole::Panel;
    const PAIR: ChordRole = ChordRole::Pairing;

    fn outcome(pause: Option<bool>, panel: PanelChange, pairing: bool) -> ChordOutcome {
        ChordOutcome {
            pause,
            panel,
            pairing,
        }
    }

    /// Every CP-1 transition of the chord state machine.
    #[test]
    fn chord_state_machine_matches_the_spec() {
        use PanelChange::*;
        // Closed.
        assert_eq!(
            on_chord(None, false, PAUSE),
            outcome(Some(true), Open(PAUSE), false)
        );
        assert_eq!(
            on_chord(None, false, PANEL),
            outcome(None, Open(PANEL), false)
        );
        assert_eq!(on_chord(None, false, PAIR), outcome(None, Keep, true));
        // Opened by the pause chord: pressing it again resumes and closes.
        assert_eq!(
            on_chord(Some(PAUSE), true, PAUSE),
            outcome(Some(false), Close, false)
        );
        // ...but if the owner resumed from the panel, it pauses and stays.
        assert_eq!(
            on_chord(Some(PAUSE), false, PAUSE),
            outcome(Some(true), Keep, false)
        );
        // Opened by the panel chord: the pause chord only toggles.
        assert_eq!(
            on_chord(Some(PANEL), false, PAUSE),
            outcome(Some(true), Keep, false)
        );
        assert_eq!(
            on_chord(Some(PANEL), true, PAUSE),
            outcome(Some(false), Keep, false)
        );
        // The panel chord always closes, resuming if paused.
        assert_eq!(
            on_chord(Some(PANEL), true, PANEL),
            outcome(Some(false), Close, false)
        );
        assert_eq!(
            on_chord(Some(PAUSE), true, PANEL),
            outcome(Some(false), Close, false)
        );
        assert_eq!(
            on_chord(Some(PANEL), false, PANEL),
            outcome(None, Close, false)
        );
        // Pairing closes an open panel (resuming) and shows the card.
        assert_eq!(
            on_chord(Some(PAUSE), true, PAIR),
            outcome(Some(false), Close, true)
        );
    }

    fn toggle(id: &str, on: bool) -> Row {
        Row::new(id, id, Control::Toggle(on))
    }

    fn sections() -> Vec<SectionRows> {
        vec![
            SectionRows {
                id: "agents",
                title: "Agents",
                rows: vec![
                    Row::new("agents.pause", "Pause agents", Control::Toggle(false))
                        .keywords(&["safe mode"]),
                    Row::new(
                        "agents.clear",
                        "Clear all",
                        Control::Action {
                            confirm: Some("Clear every agent?".into()),
                        },
                    ),
                ],
            },
            SectionRows {
                id: "empty",
                title: "Empty",
                rows: vec![],
            },
            SectionRows {
                id: "input",
                title: "Input",
                rows: vec![
                    Row::new(
                        "input.pause_chord",
                        "Pause chord",
                        Control::ReadOnly {
                            text: "Ctrl+Shift+F12".into(),
                            mono: true,
                        },
                    ),
                    Row::new(
                        "input.theme",
                        "Theme",
                        Control::Choice {
                            options: vec!["a".into(), "b".into(), "c".into()],
                            selected: 0,
                        },
                    ),
                    Row::new(
                        "input.zoom",
                        "Zoom",
                        Control::Stepper {
                            text: "100%".into(),
                        },
                    ),
                    toggle("input.extra", true),
                ],
            },
        ]
    }

    fn built(open: &mut Open) -> (ControlPanelModel, Nav) {
        build(
            open,
            &sections(),
            &ViewContext {
                paused: false,
                pause_chord: "Ctrl+Shift+F12".into(),
            },
        )
    }

    fn apply(row: &str, edit: Edit) -> Vec<Effect> {
        vec![Effect::Apply {
            row: row.into(),
            edit,
        }]
    }

    /// Empty sections are not listed; chips and Alt keys follow the listed
    /// order; the pause row becomes a tile with the chord as its shortcut.
    #[test]
    fn build_lists_sections_tiles_and_chips() {
        let mut open = Open::new(PANEL);
        let (model, nav) = built(&mut open);
        let PanelBody::Sections(cards) = &model.body else {
            panic!("sections expected")
        };
        assert_eq!(
            cards.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["agents", "input"]
        );
        assert_eq!(cards[1].key.as_deref(), Some("Alt+2"));
        assert_eq!(model.chips[1].key, "2");
        assert_eq!(nav.tiles, ["agents.pause"]);
        assert_eq!(model.tiles[0].shortcut.as_deref(), Some("Ctrl+Shift+F12"));
        assert_eq!(nav.sections, [0, 2]);
        assert_eq!(model.hint_end, "Esc close");
    }

    /// The keyboard walks search → tiles → rows in reading order and back.
    #[test]
    fn arrow_keys_walk_search_tiles_and_rows() {
        let mut open = Open::new(PANEL);
        let (_, nav) = built(&mut open);
        on_key(&mut open, &nav, PanelKey::Down, None);
        assert_eq!(open.focus, PanelFocus::Tile(0));
        on_key(&mut open, &nav, PanelKey::Down, None);
        assert_eq!(open.focus, PanelFocus::Row("agents.pause".into()));
        on_key(&mut open, &nav, PanelKey::Down, None);
        on_key(&mut open, &nav, PanelKey::Down, None);
        assert_eq!(
            open.focus,
            PanelFocus::Row("input.pause_chord".into()),
            "crosses sections"
        );
        on_key(&mut open, &nav, PanelKey::End, None);
        on_key(&mut open, &nav, PanelKey::Down, None);
        assert_eq!(
            open.focus,
            PanelFocus::Row("input.extra".into()),
            "stops at the end"
        );
        on_key(&mut open, &nav, PanelKey::Home, None);
        on_key(&mut open, &nav, PanelKey::Up, None);
        assert_eq!(open.focus, PanelFocus::Tile(0));
        on_key(&mut open, &nav, PanelKey::Up, None);
        assert_eq!(open.focus, PanelFocus::Search);
    }

    /// Tab cycles groups; Alt+N jumps to a section and scrolls it into view.
    #[test]
    fn tab_and_alt_digits_jump_between_sections() {
        let mut open = Open::new(PANEL);
        let (_, nav) = built(&mut open);
        on_key(&mut open, &nav, PanelKey::Tab, None);
        assert_eq!(open.focus, PanelFocus::Tile(0));
        on_key(&mut open, &nav, PanelKey::Tab, None);
        assert_eq!(open.focus, PanelFocus::Row("agents.pause".into()));
        on_key(&mut open, &nav, PanelKey::Tab, None);
        assert_eq!(open.focus, PanelFocus::Row("input.pause_chord".into()));
        on_key(&mut open, &nav, PanelKey::Tab, None);
        assert_eq!(open.focus, PanelFocus::Search, "wraps");
        on_key(&mut open, &nav, PanelKey::BackTab, None);
        assert_eq!(open.focus, PanelFocus::Row("input.pause_chord".into()));

        let geometry = ControlPanelGeometry {
            viewport_height: 100.0,
            max_scroll: 500.0,
            rows: vec![],
            sections: vec![
                PanelSpan {
                    id: "agents".into(),
                    top: 16.0,
                    bottom: 150.0,
                },
                PanelSpan {
                    id: "input".into(),
                    top: 300.0,
                    bottom: 500.0,
                },
            ],
            ..Default::default()
        };
        on_key(&mut open, &nav, PanelKey::Section(1), Some(&geometry));
        assert_eq!(open.focus, PanelFocus::Row("agents.pause".into()));
        on_key(&mut open, &nav, PanelKey::Section(2), Some(&geometry));
        assert_eq!(open.focus, PanelFocus::Row("input.pause_chord".into()));
        assert_eq!(open.scroll, 300.0);
        on_key(&mut open, &nav, PanelKey::Section(9), Some(&geometry));
        assert_eq!(
            open.focus,
            PanelFocus::Row("input.pause_chord".into()),
            "no such section"
        );
    }

    /// Enter/Left/Right map to each control's edit; read-only rows do nothing.
    #[test]
    fn keys_edit_controls() {
        let mut open = Open::new(PANEL);
        let (_, nav) = built(&mut open);
        let focus = |open: &mut Open, id: &str| open.focus = PanelFocus::Row(id.into());
        focus(&mut open, "agents.pause");
        assert_eq!(
            on_key(&mut open, &nav, PanelKey::Enter, None),
            apply("agents.pause", Edit::Set(true))
        );
        assert_eq!(
            on_key(&mut open, &nav, PanelKey::Left, None),
            apply("agents.pause", Edit::Set(false))
        );
        focus(&mut open, "input.theme");
        assert_eq!(
            on_key(&mut open, &nav, PanelKey::Left, None),
            apply("input.theme", Edit::Choose(2))
        );
        assert_eq!(
            on_key(&mut open, &nav, PanelKey::Space, None),
            apply("input.theme", Edit::Choose(1))
        );
        focus(&mut open, "input.zoom");
        assert_eq!(
            on_key(&mut open, &nav, PanelKey::Right, None),
            apply("input.zoom", Edit::Step(1))
        );
        assert!(on_key(&mut open, &nav, PanelKey::Enter, None).is_empty());
        focus(&mut open, "input.pause_chord");
        assert!(on_key(&mut open, &nav, PanelKey::Enter, None).is_empty());
        // Quick-action accelerators reach rows that exist, and only those.
        assert_eq!(
            on_key(&mut open, &nav, PanelKey::Alt('t'), None),
            Vec::new(),
            "no theme row yet"
        );
    }

    /// A confirmed action asks first; Esc or moving away cancels.
    #[test]
    fn confirmed_actions_ask_once_and_cancel_on_escape() {
        let mut open = Open::new(PANEL);
        let (_, nav) = built(&mut open);
        open.focus = PanelFocus::Row("agents.clear".into());
        assert!(on_key(&mut open, &nav, PanelKey::Enter, None).is_empty());
        assert_eq!(open.confirming.as_deref(), Some("agents.clear"));
        let (model, _) = built(&mut open);
        let PanelBody::Sections(cards) = &model.body else {
            panic!()
        };
        let row = &cards[0].items[1];
        assert_eq!(row.detail.as_deref(), Some("Clear every agent?"));
        assert_eq!(row.emphasis, PanelTone::Danger);
        assert_eq!(
            on_key(&mut open, &nav, PanelKey::Enter, None),
            apply("agents.clear", Edit::Activate)
        );
        assert_eq!(open.confirming, None);

        on_key(&mut open, &nav, PanelKey::Enter, None);
        assert!(
            on_key(&mut open, &nav, PanelKey::Escape, None).is_empty(),
            "Esc cancels, not closes"
        );
        assert_eq!(open.confirming, None);
        on_key(&mut open, &nav, PanelKey::Enter, None);
        on_key(&mut open, &nav, PanelKey::Up, None);
        assert_eq!(open.confirming, None, "moving away cancels");
        // A click on the same row confirms; on another row cancels.
        let target = PanelHitTarget::Row("agents.clear".into(), PanelPart::Body);
        assert!(on_click(&mut open, &nav, &target, None).is_empty());
        assert_eq!(
            on_click(&mut open, &nav, &target, None),
            apply("agents.clear", Edit::Activate)
        );
    }

    /// Typing searches across sections; Enter acts on the best match; with no
    /// match the body says so and Esc clears before it closes.
    #[test]
    fn typing_searches_and_escape_clears_then_closes() {
        let mut open = Open::new(PANEL);
        let (_, nav) = built(&mut open);
        on_key(&mut open, &nav, PanelKey::Down, None);
        on_text(&mut open, "pa");
        let (model, nav) = built(&mut open);
        let PanelBody::Results { title, items } = &model.body else {
            panic!("results expected")
        };
        assert_eq!(items[0].id, "agents.pause", "label prefix ranks first");
        assert_eq!(items[0].context.as_deref(), Some("Agents"));
        assert!(title.starts_with(&format!("{} results", items.len())));
        assert_eq!(
            open.focus,
            PanelFocus::Row("agents.pause".into()),
            "best match focused"
        );
        assert_eq!(model.hint_end, "Esc clear search");
        open.focus = PanelFocus::Search;
        assert_eq!(
            on_key(&mut open, &nav, PanelKey::Enter, None),
            apply("agents.pause", Edit::Set(true))
        );

        on_text(&mut open, "zzz");
        let (model, _) = built(&mut open);
        assert_eq!(
            model.body,
            PanelBody::Empty("No settings match “pazzz”. Esc clears the search.".into())
        );
        assert_eq!(open.focus, PanelFocus::Search);
        on_key(&mut open, &nav, PanelKey::Backspace, None);
        assert_eq!(open.query, "pazz");
        assert!(on_key(&mut open, &nav, PanelKey::Escape, None).is_empty());
        assert_eq!(open.query, "");
        assert_eq!(
            on_key(&mut open, &nav, PanelKey::Escape, None),
            vec![Effect::Close]
        );
        // Leading spaces and control characters are not typed.
        on_text(&mut open, " \u{8}");
        assert_eq!(open.query, "");
    }

    /// Focus survives a rebuild when its row is still there and falls back to
    /// search when it is gone.
    #[test]
    fn focus_is_repaired_when_its_row_disappears() {
        let mut open = Open::new(PANEL);
        open.focus = PanelFocus::Row("agents.gone".into());
        built(&mut open);
        assert_eq!(open.focus, PanelFocus::Search);
        open.focus = PanelFocus::Tile(7);
        built(&mut open);
        assert_eq!(open.focus, PanelFocus::Search);
    }

    /// Clicks on parts step and cycle; the wheel scrolls within bounds; a
    /// failed edit shows on its row.
    #[test]
    fn clicks_wheel_and_errors() {
        let mut open = Open::new(PANEL);
        let (_, nav) = built(&mut open);
        let click = |open: &mut Open, id: &str, part| {
            on_click(open, &nav, &PanelHitTarget::Row(id.into(), part), None)
        };
        assert_eq!(
            click(&mut open, "input.zoom", PanelPart::Decrement),
            apply("input.zoom", Edit::Step(-1))
        );
        assert_eq!(
            click(&mut open, "input.theme", PanelPart::Body),
            apply("input.theme", Edit::Choose(1))
        );
        assert_eq!(open.focus, PanelFocus::Row("input.theme".into()));
        assert_eq!(
            on_click(
                &mut open,
                &nav,
                &PanelHitTarget::Tile(0, PanelPart::Body),
                None
            ),
            apply("agents.pause", Edit::Set(true))
        );
        let g = ControlPanelGeometry {
            max_scroll: 50.0,
            ..Default::default()
        };
        on_scroll(&mut open, 80.0, Some(&g));
        assert_eq!(open.scroll, 50.0);
        on_scroll(&mut open, -500.0, Some(&g));
        assert_eq!(open.scroll, 0.0);

        open.error = Some(("input.theme".into(), "config.toml is read-only".into()));
        let (model, _) = built(&mut open);
        let PanelBody::Sections(cards) = &model.body else {
            panic!()
        };
        let theme = cards[1]
            .items
            .iter()
            .find(|i| i.id == "input.theme")
            .unwrap();
        assert_eq!(theme.detail.as_deref(), Some("config.toml is read-only"));
        assert_eq!(theme.detail_tone, PanelTone::Danger);
    }

    /// A search that matches nothing leaves no rows, but the quick tiles
    /// still act, and paging against the previous frame's geometry (which
    /// still lists rows) is a no-op rather than an out-of-range index.
    #[test]
    fn tiles_act_and_paging_is_safe_while_a_search_matches_nothing() {
        let mut open = Open::new(PANEL);
        on_text(&mut open, "zzz");
        let (model, nav) = built(&mut open);
        assert!(matches!(model.body, PanelBody::Empty(_)) && nav.rows.is_empty());
        let tile = PanelHitTarget::Tile(0, PanelPart::Body);
        assert_eq!(
            on_click(&mut open, &nav, &tile, None),
            apply("agents.pause", Edit::Set(true))
        );
        let stale = ControlPanelGeometry {
            viewport_height: 400.0,
            rows: vec![PanelSpan {
                id: "agents.pause".into(),
                top: 0.0,
                bottom: 48.0,
            }],
            ..Default::default()
        };
        for key in [
            PanelKey::PageDown,
            PanelKey::PageUp,
            PanelKey::End,
            PanelKey::Home,
        ] {
            assert!(on_key(&mut open, &nav, key, Some(&stale)).is_empty());
        }
    }
}
