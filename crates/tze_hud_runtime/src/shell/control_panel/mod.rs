//! HUD control panel (T11, hud-vniri): one scrollable pane of glass for every
//! HUD setting, opened by a global chord.
//!
//! ## Shape
//!
//! - **Sections** ([`Section`], `sections/`) read live facts into [`Row`]s and
//!   apply [`Edit`]s. They are the only place that knows what a setting means.
//! - **The core** (`state.rs`) is pure: the chord state machine, the view model,
//!   search, focus, confirmation and keyboard/pointer handling.
//! - **The task** (this file) owns the open state on the network runtime. It
//!   receives [`PanelEvent`]s (chords from the hotkey thread; keys, text,
//!   clicks and wheel from the winit loop), runs the resulting edits, rebuilds
//!   the model from the sections, and publishes it through the
//!   [`ControlPanelHandle`]. A chord therefore never waits on the winit loop or
//!   an agent (docs/invariants.md §3).
//! - **The compositor** lays the model out from theme tokens and draws it in the
//!   final chrome pass; the geometry it drew comes back through the handle for
//!   pointer hits and scrolling.
//!
//! The panel lives outside the scene graph like the system card: no agent API
//! can observe it, and `/admin/screenshot` leaves it out.
//!
//! ## Extending
//!
//! - A new setting: add a [`Row`] to the section that owns the area and handle
//!   its [`Edit`] in that section's `apply`. Search, keyboard, pointer, focus,
//!   confirmation and error display come for free.
//! - A new area: implement [`Section`] and add it to [`default_sections`].
//! - Data-driven rows (one per theme, widget, agent...) are built in `rows`
//!   from the registry that owns them, so adding a theme or widget adds its
//!   row with no panel change.
//! - A quick tile: add the row id to `state::QUICK_ACTIONS`.
//!
//! Cost: nothing runs while the panel is closed except the chord wait; while
//! open and idle nothing redraws (the model is republished only when it
//! changes).

pub(crate) mod agents;
pub(crate) mod chords;
pub(crate) mod connect;
mod pause;
pub(crate) mod row;
pub(crate) mod runtime;
mod search;
mod sections;
mod state;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use tze_hud_compositor::{ControlPanelGeometry, ControlPanelModel, PanelHitTarget, PanelNotice};
use tze_hud_scene::config::{ChordRole, HudChords};
use tze_hud_scene::render_wake::RenderWakeNotifier;

pub(crate) use agents::AgentControl;
pub(crate) use connect::ConnectControl;
pub(crate) use pause::PauseControl;
pub(crate) use row::Section;
pub(crate) use runtime::RuntimeControl;
pub(crate) use state::PanelKey;

use crate::threads::ShutdownToken;
use row::{Edit, Row};
use state::{Effect, Nav, Open, PanelChange, SectionRows, ViewContext};

/// Input to the panel task.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PanelEvent {
    /// A global chord was pressed (hotkey thread).
    #[cfg_attr(
        not(any(target_os = "windows", test, feature = "test-harness")),
        expect(dead_code, reason = "chords come from the Windows hotkey thread")
    )]
    Chord(ChordRole),
    Key(PanelKey),
    /// Typed text (search).
    Text(String),
    /// Press and release on the same target.
    Click(PanelHitTarget),
    Hover(Option<PanelHitTarget>),
    /// Wheel, in px (> 0 scrolls down).
    Scroll(f32),
    /// The panel window gained (`true`) or lost keyboard focus.
    WindowFocus(bool),
    /// Re-read the sections (sent once a second while open, so facts that
    /// change outside the panel, like an agent connecting, show up).
    Refresh,
    /// The compositor laid out a row the panel asked to scroll into view.
    Geometry,
    /// A slow edit finished (sent by the panel task itself).
    Applied {
        row: String,
        result: Result<(), String>,
    },
}

struct Shared {
    /// What the compositor draws (`None`: closed), with a version that changes
    /// on every publish.
    model: RwLock<(u64, Option<ControlPanelModel>)>,
    /// The notice the compositor draws while the panel is closed, versioned
    /// like `model`.
    closed_notice: RwLock<(u64, Option<PanelNotice>)>,
    open: AtomicBool,
    /// The geometry the compositor last drew, with its layout generation.
    geometry: RwLock<Option<(u64, ControlPanelGeometry)>>,
    geometry_generation: AtomicU64,
    /// The panel waits for new geometry to scroll a row into view.
    wants_geometry: AtomicBool,
    events: mpsc::UnboundedSender<PanelEvent>,
    render_wake: RenderWakeNotifier,
    /// Wakes the winit loop when new geometry arrives, so the overlay's
    /// click-through state follows the panel's drawn rect.
    main_wake: RenderWakeNotifier,
    /// Background edits whose outcome was queued (tests order on it).
    #[cfg(test)]
    applied_sent: AtomicU64,
}

/// Cheap, cloneable access to the panel from every thread.
#[derive(Clone)]
pub(crate) struct ControlPanelHandle {
    shared: Arc<Shared>,
}

impl ControlPanelHandle {
    /// Queue an event for the panel task; false once the task has stopped.
    pub(crate) fn send(&self, event: PanelEvent) -> bool {
        self.shared.events.send(event).is_ok()
    }

    /// How many background edits have queued their outcome.
    #[cfg(test)]
    pub(crate) fn applied_sent(&self) -> u64 {
        self.shared.applied_sent.load(Ordering::SeqCst)
    }

    pub(crate) fn is_open(&self) -> bool {
        self.shared.open.load(Ordering::Acquire)
    }

    /// For the compositor thread: the model when it changed since `seen`
    /// (updating `seen`), else `None`.
    pub(crate) fn model_if_changed(&self, seen: &mut u64) -> Option<Option<ControlPanelModel>> {
        let guard = self.shared.model.read().unwrap_or_else(|e| e.into_inner());
        if guard.0 == *seen {
            return None;
        }
        *seen = guard.0;
        Some(guard.1.clone())
    }

    /// For the compositor thread: the closed-panel notice when it changed since
    /// `seen` (updating `seen`), else `None`.
    pub(crate) fn closed_notice_if_changed(&self, seen: &mut u64) -> Option<Option<PanelNotice>> {
        let guard = self
            .shared
            .closed_notice
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if guard.0 == *seen {
            return None;
        }
        *seen = guard.0;
        Some(guard.1.clone())
    }

    /// The closed-panel notice last published.
    #[cfg(test)]
    pub(crate) fn current_closed_notice(&self) -> Option<PanelNotice> {
        self.shared
            .closed_notice
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .clone()
    }

    fn publish_closed_notice(&self, notice: Option<PanelNotice>) {
        {
            let mut guard = self
                .shared
                .closed_notice
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if guard.1 == notice {
                return;
            }
            guard.0 += 1;
            guard.1 = notice;
        }
        self.shared.render_wake.notify();
    }

    /// For the compositor thread: record the geometry it drew. Cheap when the
    /// layout generation did not change.
    pub(crate) fn set_geometry(
        &self,
        generation: u64,
        geometry: impl FnOnce() -> ControlPanelGeometry,
    ) {
        if self
            .shared
            .geometry_generation
            .swap(generation, Ordering::AcqRel)
            == generation
        {
            return;
        }
        *self
            .shared
            .geometry
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some((generation, geometry()));
        self.shared.main_wake.notify();
        if self.shared.wants_geometry.swap(false, Ordering::AcqRel) {
            self.send(PanelEvent::Geometry);
        }
    }

    fn geometry(&self) -> Option<ControlPanelGeometry> {
        self.shared
            .geometry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(_, g)| g.clone())
    }

    /// What the panel has at `(x, y)` on the primary display; `None` when the
    /// panel is closed or the point is outside it (it passes through).
    pub(crate) fn hit(&self, x: f32, y: f32) -> Option<PanelHitTarget> {
        if !self.is_open() {
            return None;
        }
        self.shared
            .geometry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|(_, g)| g.hit(x, y))
    }

    /// The model last published (`None`: closed).
    #[cfg(any(test, feature = "test-harness"))]
    pub(crate) fn current_model(&self) -> Option<ControlPanelModel> {
        self.shared
            .model
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .clone()
    }

    fn publish(&self, model: Option<ControlPanelModel>) {
        let open = model.is_some();
        {
            let mut guard = self.shared.model.write().unwrap_or_else(|e| e.into_inner());
            guard.0 += 1;
            guard.1 = model;
        }
        if !open {
            *self
                .shared
                .geometry
                .write()
                .unwrap_or_else(|e| e.into_inner()) = None;
        }
        self.shared.open.store(open, Ordering::Release);
        self.shared.render_wake.notify();
    }
}

/// What the panel task needs besides its sections.
#[derive(Clone)]
pub(crate) struct ControlPanelDeps {
    pub pause: PauseControl,
    /// The scene, for the visibility rows (its runtime-only overlay state).
    pub scene: Arc<tokio::sync::Mutex<tze_hud_scene::graph::SceneGraph>>,
    pub chords: chords::ChordControl,
    /// Shows the pairing card (`None` when the MCP port is disabled).
    pub pairing: Option<Arc<crate::pairing::Pairing>>,
    /// The Agents section's roster and actions (`None`: pause only).
    pub agents: Option<AgentControl>,
    /// Connect an agent (`None` when the MCP port is disabled).
    pub connect: Option<ConnectControl>,
    /// Launch settings, restart, update and the rest of Network,
    /// Diagnostics and System (`None`: those show only the version).
    pub runtime: Option<RuntimeControl>,
    /// Live theme, fonts and tokens (`None`: no Appearance or Advanced
    /// tokens cards).
    pub appearance: Option<Arc<crate::appearance::Appearance>>,
    /// Wakes the compositor (and the winit loop) after a publish.
    pub render_wake: RenderWakeNotifier,
    /// Wakes only the winit loop (new geometry changes pointer capture).
    pub main_wake: RenderWakeNotifier,
}

/// The sections every HUD shows, in display order.
pub(crate) fn default_sections(deps: &ControlPanelDeps) -> Vec<Box<dyn Section>> {
    let mut sections: Vec<Box<dyn Section>> = vec![
        Box::new(sections::AgentsSection::new(
            deps.pause.clone(),
            deps.agents.clone(),
            deps.connect.is_some(),
            deps.runtime.clone(),
        )),
        Box::new(sections::VisibilitySection::new(
            Arc::clone(&deps.scene),
            deps.render_wake.clone(),
        )),
    ];
    if let Some(appearance) = &deps.appearance {
        sections.push(Box::new(sections::AppearanceSection::new(
            Arc::clone(appearance),
            deps.runtime.as_ref().and_then(|r| r.write_tokens.clone()),
        )));
    }
    sections.extend::<[Box<dyn Section>; 2]>([
        Box::new(sections::LayoutSection::new(
            Arc::clone(&deps.scene),
            deps.render_wake.clone(),
            deps.runtime.as_ref().and_then(|r| r.write_widget.clone()),
            deps.runtime
                .as_ref()
                .map(|r| r.displays.clone())
                .unwrap_or_default(),
        )),
        Box::new(sections::InputSection::new(
            deps.chords.clone(),
            deps.pairing.is_some(),
            deps.runtime
                .as_ref()
                .filter(|r| r.config_path.is_some())
                .map(|r| r.write.clone()),
        )),
    ]);
    if let Some(runtime) = &deps.runtime {
        sections.push(Box::new(sections::NetworkSection::new(runtime.clone())));
        sections.push(Box::new(sections::DiagnosticsSection::new(runtime.clone())));
    }
    sections.push(Box::new(sections::SystemSection::new(deps.runtime.clone())));
    if let Some(appearance) = &deps.appearance {
        sections.push(Box::new(sections::TokensSection::new(
            Arc::clone(appearance),
            deps.runtime.as_ref().and_then(|r| r.write_tokens.clone()),
        )));
    }
    sections
}

/// Start the panel task on `runtime` and return its handle.
///
/// The panel carries the human override, so a bug in one event handler (a
/// section's `rows` or `apply`, or the core) must not wedge it: each event
/// runs in its own task, and a panic closes the panel, resumes agents and
/// starts over from a fresh state. Events arrive at human speed, so a spawn
/// per event costs nothing that matters.
pub(crate) fn spawn_control_panel(
    runtime: &tokio::runtime::Handle,
    deps: ControlPanelDeps,
    sections: Vec<Box<dyn Section>>,
    shutdown: ShutdownToken,
) -> ControlPanelHandle {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let handle = ControlPanelHandle {
        shared: Arc::new(Shared {
            model: RwLock::new((0, None)),
            closed_notice: RwLock::new((0, None)),
            open: AtomicBool::new(false),
            geometry: RwLock::new(None),
            geometry_generation: AtomicU64::new(0),
            wants_geometry: AtomicBool::new(false),
            events: tx,
            render_wake: deps.render_wake.clone(),
            main_wake: deps.main_wake.clone(),
            #[cfg(test)]
            applied_sent: AtomicU64::new(0),
        }),
    };
    let sections: Arc<[Box<dyn Section>]> = sections.into();
    let fresh = {
        let (handle, deps) = (handle.clone(), deps.clone());
        move || PanelTask {
            handle: handle.clone(),
            deps: deps.clone(),
            sections: Arc::clone(&sections),
            open: None,
            nav: Nav::default(),
            published: None,
            busy: std::collections::HashSet::new(),
            after_busy: std::collections::HashMap::new(),
            changes: std::collections::HashMap::new(),
            closed_failure: None,
        }
    };
    let recover = handle.clone();
    let open = handle.clone();
    runtime.spawn(async move {
        let mut shutdown_rx = shutdown.subscribe();
        let mut task = Some(fresh());
        let mut refresh = tokio::time::interval(REFRESH_INTERVAL);
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                event = rx.recv() => {
                    let Some(event) = event else { break };
                    let mut current = task.take().unwrap_or_else(&fresh);
                    let run = tokio::spawn(async move {
                        current.handle(event).await;
                        current
                    });
                    task = match run.await {
                        Ok(current) => Some(current),
                        // Runtime shutdown cancels the task: nothing to recover.
                        Err(error) if error.is_cancelled() => break,
                        Err(error) => {
                            tracing::error!(
                                %error,
                                "control panel: event handler failed; closing the panel and resuming agents"
                            );
                            recover.publish(None);
                            deps.pause.set(false).await;
                            None
                        }
                    };
                }
                // While open only: a closed panel costs nothing.
                _ = refresh.tick(), if open.is_open() => {
                    open.send(PanelEvent::Refresh);
                }
                _ = shutdown_rx.recv() => break,
            }
        }
    });
    handle
}

/// How often an open panel re-reads its sections. The model is republished
/// (and redrawn) only when something changed.
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// How long an edit may run before the panel stops waiting for it. A slower
/// one (an administrator prompt, an update download) finishes in the
/// background with its row marked busy, so keys, chords and Esc (which
/// resumes agents) never wait on it.
const APPLY_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

struct PanelTask {
    handle: ControlPanelHandle,
    deps: ControlPanelDeps,
    sections: Arc<[Box<dyn Section>]>,
    open: Option<Open>,
    nav: Nav,
    published: Option<ControlPanelModel>,
    /// Rows whose edit is still running in the background. Kept across
    /// close and reopen so a second edit cannot start beside the first.
    busy: std::collections::HashSet<String>,
    /// A previewing row's edits (more steps, keep, revert) sent while its
    /// last one still runs, in order: dropping one would leave an unsaved
    /// theme on screen or lose the one kept.
    after_busy: std::collections::HashMap<String, std::collections::VecDeque<Edit>>,
    /// Each running edit's (old, new) value, for its outcome's log line.
    changes: std::collections::HashMap<String, (String, String)>,
    /// A background edit that failed while the panel was closed, shown on
    /// its row when the panel next opens.
    closed_failure: Option<(String, String)>,
}

impl PanelTask {
    async fn handle(&mut self, event: PanelEvent) {
        let effects = match (event, self.open.as_mut()) {
            (PanelEvent::Applied { row, result }, _) => {
                self.busy.remove(&row);
                self.applied(&row, result);
                let next = self.after_busy.get_mut(&row).and_then(|q| q.pop_front());
                if self.after_busy.get(&row).is_some_and(|q| q.is_empty()) {
                    self.after_busy.remove(&row);
                }
                match next {
                    Some(edit) => vec![Effect::Apply { row, edit }],
                    None => Vec::new(),
                }
            }
            (PanelEvent::Chord(role), _) => {
                self.on_chord(role).await;
                Vec::new()
            }
            // Input that arrives after the panel closed is stale.
            (_, None) => return,
            (event, Some(open)) => {
                let geometry = self.handle.geometry();
                match event {
                    PanelEvent::Key(key) => state::on_key(open, &self.nav, key, geometry.as_ref()),
                    PanelEvent::Text(text) => {
                        state::on_text(open, &self.nav, &text);
                        Vec::new()
                    }
                    PanelEvent::Click(target) => {
                        state::on_click(open, &self.nav, &target, geometry.as_ref())
                    }
                    PanelEvent::Hover(target) => {
                        open.hover = target;
                        Vec::new()
                    }
                    PanelEvent::Scroll(dy) => {
                        state::on_scroll(open, dy, geometry.as_ref());
                        Vec::new()
                    }
                    PanelEvent::WindowFocus(focused) => {
                        open.focus_refused = !focused;
                        Vec::new()
                    }
                    PanelEvent::Refresh => {
                        self.refresh().await;
                        Vec::new()
                    }
                    PanelEvent::Geometry => {
                        if open.reveal
                            && let Some(g) = &geometry
                        {
                            state::reveal_focus(open, g);
                        }
                        Vec::new()
                    }
                    PanelEvent::Chord(_) | PanelEvent::Applied { .. } => Vec::new(),
                }
            }
        };
        for effect in effects {
            match effect {
                Effect::Close => self.close().await,
                Effect::Apply { row, edit } => self.apply(&row, edit).await,
            }
        }
        self.rebuild().await;
        // Focus left a previewing row (a key, a click, or a search moved
        // it): back to the saved option.
        if let Some(row) = self
            .open
            .as_mut()
            .and_then(|open| open.left_preview(&self.nav))
        {
            self.apply(&row, Edit::Revert).await;
            self.rebuild().await;
        }
        if self.open.as_ref().is_some_and(|open| open.reveal) {
            // Ask first, then look: geometry drawn in between still wakes us.
            self.handle
                .shared
                .wants_geometry
                .store(true, Ordering::Release);
            if let (Some(open), Some(g)) = (self.open.as_mut(), self.handle.geometry())
                && state::reveal_focus(open, &g)
            {
                self.handle
                    .shared
                    .wants_geometry
                    .store(false, Ordering::Release);
                self.rebuild().await;
            }
        }
    }

    async fn on_chord(&mut self, role: ChordRole) {
        let paused = self.deps.pause.is_paused();
        let outcome = state::on_chord(self.open.as_ref().map(|o| o.opened_by), paused, role);
        tracing::info!(chord = role.config_key(), ?outcome, "control panel: chord");
        if let Some(pause) = outcome.pause {
            self.deps.pause.set(pause).await;
        }
        match outcome.panel {
            PanelChange::Open(by) => {
                let mut open = Open::new(by);
                open.error = self.closed_failure.take();
                self.open = Some(open);
                for section in self.sections.iter() {
                    section.opened();
                }
                self.refresh().await;
            }
            PanelChange::Close => self.end_preview().await,
            PanelChange::Keep => {}
        }
        if outcome.connect {
            self.open_connect();
        }
    }

    /// Show Connect an agent with a fresh one-time code. Without the
    /// panel's Connect (no network runtime), the pairing card shows instead.
    fn open_connect(&mut self) {
        match (&self.deps.connect, self.open.as_mut()) {
            (Some(control), Some(open)) => {
                open.connect = Some(control.open());
                open.query.clear();
                open.confirming = None;
                open.scroll = 0.0;
                open.focus = tze_hud_compositor::PanelFocus::Row(connect::NAME.to_owned());
            }
            _ => {
                if let Some(pairing) = &self.deps.pairing {
                    pairing.open();
                }
            }
        }
    }

    /// Run a Connect row's action; `false` when `row` is not one.
    async fn apply_connect(&mut self, row: &str) -> bool {
        if row == connect::OPEN {
            self.open_connect();
            return true;
        }
        let Some(control) = self.deps.connect.clone() else {
            return false;
        };
        let Some(page) = self.open.as_mut().and_then(|o| o.connect.as_mut()) else {
            return false;
        };
        match row {
            connect::NAME | connect::COPY => control.copy(page).await,
            connect::NEW_CODE => control.new_code(page),
            _ => return false,
        }
        true
    }

    /// Close the panel; closing always resumes agents.
    async fn close(&mut self) {
        self.end_preview().await;
        self.deps.pause.set(false).await;
    }

    /// Forget the open panel, reverting a preview it was showing.
    async fn end_preview(&mut self) {
        if let Some(row) = self.open.take().and_then(|open| open.preview) {
            self.apply(&row, Edit::Revert).await;
        }
    }

    async fn apply(&mut self, row: &str, edit: Edit) {
        if edit == Edit::Activate && self.apply_connect(row).await {
            return;
        }
        let section_id = row.split('.').next().unwrap_or_default();
        let Some(index) = self.sections.iter().position(|s| s.id() == section_id) else {
            tracing::warn!(row, "control panel: no section owns this row");
            return;
        };
        if self.busy.contains(row) {
            if self.nav.previews.iter().any(|r| r == row) || edit == Edit::Revert {
                let queue = self.after_busy.entry(row.to_owned()).or_default();
                match (queue.back_mut(), edit) {
                    // Key repeat: one restyle for all the steps queued.
                    (Some(Edit::Preview(m)), Edit::Preview(n)) => *m += n,
                    // A revert makes previews still waiting moot.
                    (_, Edit::Revert) => {
                        while matches!(queue.back(), Some(Edit::Preview(_))) {
                            queue.pop_back();
                        }
                        queue.push_back(Edit::Revert);
                    }
                    (_, edit) => queue.push_back(edit),
                }
                return;
            }
            tracing::info!(
                row,
                "control panel: edit ignored; the last one is still running"
            );
            return;
        }
        let change = self
            .nav
            .control(row)
            .map(|control| (control.value_text(), control.edited_text(&edit)))
            .unwrap_or_default();
        tracing::debug!(row, ?edit, "control panel: applying");
        self.changes.insert(row.to_owned(), change);
        // Isolated: a section that panics fails its row, not the panel.
        let (sections, owned_row) = (Arc::clone(&self.sections), row.to_owned());
        let mut run = tokio::spawn(async move { sections[index].apply(&owned_row, edit).await });
        let joined = |joined: Result<Result<(), String>, tokio::task::JoinError>| {
            joined.unwrap_or_else(|error| {
                tracing::error!(%error, "control panel: section apply failed");
                Err("This setting failed; see the HUD log".to_owned())
            })
        };
        match tokio::time::timeout(APPLY_WAIT, &mut run).await {
            Ok(result) => self.applied(row, joined(result)),
            Err(_) => {
                self.busy.insert(row.to_owned());
                let (handle, row) = (self.handle.clone(), row.to_owned());
                tokio::spawn(async move {
                    let result = joined(run.await);
                    handle.send(PanelEvent::Applied { row, result });
                    #[cfg(test)]
                    handle.shared.applied_sent.fetch_add(1, Ordering::SeqCst);
                });
            }
        }
    }

    /// Log an edit's outcome (one line, with the row's value before and what
    /// the edit set) and show a failure on its row.
    fn applied(&mut self, row: &str, result: Result<(), String>) {
        let (old, new) = self.changes.remove(row).unwrap_or_default();
        match result {
            Ok(()) => tracing::info!(row, old, new, "control panel: applied"),
            Err(message) => {
                tracing::warn!(row, old, new, %message, "control panel: edit failed");
                let failure = Some((row.to_owned(), message));
                match self.open.as_mut() {
                    Some(open) => open.error = failure,
                    None => self.closed_failure = failure,
                }
            }
        }
    }

    /// Let every section re-read its outside state.
    async fn refresh(&self) {
        for (index, section) in self.sections.iter().enumerate() {
            // Isolated, like `rebuild`: a failed refresh keeps the last rows.
            let sections = Arc::clone(&self.sections);
            if let Err(error) = tokio::spawn(async move { sections[index].refresh().await }).await {
                tracing::error!(section = section.id(), %error, "control panel: section refresh failed");
            }
        }
    }

    /// Rebuild the model from the sections and publish it if it changed.
    async fn rebuild(&mut self) {
        let model = match self.open.as_mut() {
            None => None,
            Some(open) => {
                let mut rows = Vec::with_capacity(self.sections.len());
                let mut summaries = Vec::new();
                for (index, section) in self.sections.iter().enumerate() {
                    // Isolated: a section that panics shows one error row
                    // and the rest of the panel (and any pause) stands.
                    let sections = Arc::clone(&self.sections);
                    let (section_rows, summary) = tokio::spawn(async move {
                        let section = &sections[index];
                        (section.rows().await, section.summary())
                    })
                    .await
                    .unwrap_or_else(|error| {
                        tracing::error!(
                            section = section.id(),
                            %error,
                            "control panel: section rows failed"
                        );
                        let failed = Row::new(
                            format!("{}.unavailable", section.id()),
                            "This section failed to load",
                            row::Control::ReadOnly {
                                text: "See the HUD log".to_owned(),
                                mono: false,
                            },
                        )
                        .caution(true);
                        (vec![failed], None)
                    });
                    summaries.extend(summary);
                    let mut section_rows = section_rows;
                    for row in section_rows
                        .iter_mut()
                        .filter(|r| self.busy.contains(&r.id))
                    {
                        row.detail = Some("Working… (Esc closes; this carries on)".to_owned());
                    }
                    rows.push(SectionRows {
                        id: section.id(),
                        title: section.title(),
                        rows: section_rows,
                    });
                }
                let ctx = ViewContext {
                    paused: self.deps.pause.is_paused(),
                    state_line: summaries.join(" · "),
                    pause_chord: self.deps.chords.get().pause.to_string(),
                    connect: open.connect.as_ref().zip(self.deps.connect.as_ref()).map(
                        |(page, control)| {
                            let remote_row = rows
                                .iter()
                                .flat_map(|s| &s.rows)
                                .any(|r| r.id == sections::NetworkSection::REMOTE);
                            control.view(page, remote_row)
                        },
                    ),
                };
                let (model, nav) = state::build(open, &rows, &ctx);
                self.nav = nav;
                Some(model)
            }
        };
        let closed = model.is_none();
        // Off the Connect card, a replacement code shows on the system card.
        if let Some(control) = &self.deps.connect
            && self.open.as_ref().is_none_or(|o| o.connect.is_none())
        {
            control.leave();
        }
        if model != self.published {
            self.published = model.clone();
            self.handle.publish(model);
        }
        if closed {
            let notice = self.closed_notice().await;
            self.handle.publish_closed_notice(notice);
        }
    }

    /// The sections' notices joined, with the chord that reopens the panel;
    /// `None` when no section has one.
    async fn closed_notice(&self) -> Option<PanelNotice> {
        let mut parts = Vec::new();
        for index in 0..self.sections.len() {
            let sections = Arc::clone(&self.sections);
            match tokio::spawn(async move { sections[index].closed_notice().await }).await {
                Ok(Some(notice)) => parts.push(notice),
                Ok(None) => {}
                Err(error) => tracing::error!(
                    section = self.sections[index].id(),
                    %error,
                    "control panel: section notice failed"
                ),
            }
        }
        if parts.is_empty() {
            return None;
        }
        let panel_chord = crate::operator::status::hotkey(ChordRole::Panel);
        Some(PanelNotice {
            text: parts.join(" · "),
            key: reopen_chord(&panel_chord, &self.deps.chords.get()).to_string(),
        })
    }
}

/// The chord that opens the panel: the panel chord, or the pause chord when
/// Windows refused the panel chord.
fn reopen_chord(
    panel_chord: &crate::operator::status::HotkeyStatus,
    chords: &HudChords,
) -> tze_hud_scene::config::Hotkey {
    match panel_chord {
        crate::operator::status::HotkeyStatus::Failed { .. } => chords.pause,
        _ => chords.panel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator::status::HotkeyStatus;

    /// The closed-panel notice names the panel chord, or the pause chord when
    /// Windows refused the panel chord (CP-6).
    #[test]
    fn reopen_chord_falls_back_to_the_pause_chord() {
        let chords = HudChords::default();
        let failed = HotkeyStatus::Failed {
            chord: chords.panel.to_string(),
            reason: "in use".into(),
        };
        assert_eq!(reopen_chord(&failed, &chords), chords.pause);
        assert_eq!(
            reopen_chord(&HotkeyStatus::NotApplicable, &chords),
            chords.panel
        );
    }
}
