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

mod pause;
pub(crate) mod row;
mod search;
mod sections;
mod state;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use tze_hud_compositor::{ControlPanelGeometry, ControlPanelModel, PanelHitTarget};
use tze_hud_scene::config::{ChordRole, HudChords};
use tze_hud_scene::render_wake::RenderWakeNotifier;

pub(crate) use pause::PauseControl;
pub(crate) use row::Section;
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
}

struct Shared {
    /// What the compositor draws (`None`: closed), with a version that changes
    /// on every publish.
    model: RwLock<(u64, Option<ControlPanelModel>)>,
    /// The notice the compositor draws while the panel is closed, versioned
    /// like `model`.
    closed_notice: RwLock<(u64, Option<String>)>,
    open: AtomicBool,
    /// The geometry the compositor last drew, with its layout generation.
    geometry: RwLock<Option<(u64, ControlPanelGeometry)>>,
    geometry_generation: AtomicU64,
    events: mpsc::UnboundedSender<PanelEvent>,
    render_wake: RenderWakeNotifier,
    /// Wakes the winit loop when new geometry arrives, so the overlay's
    /// click-through state follows the panel's drawn rect.
    main_wake: RenderWakeNotifier,
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
    pub(crate) fn closed_notice_if_changed(&self, seen: &mut u64) -> Option<Option<String>> {
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
    pub(crate) fn current_closed_notice(&self) -> Option<String> {
        self.shared
            .closed_notice
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .clone()
    }

    fn publish_closed_notice(&self, notice: Option<String>) {
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
    #[cfg(test)]
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
    pub chords: HudChords,
    /// Shows the pairing card (`None` when the MCP port is disabled).
    pub pairing: Option<Arc<crate::pairing::Pairing>>,
    /// Wakes the compositor (and the winit loop) after a publish.
    pub render_wake: RenderWakeNotifier,
    /// Wakes only the winit loop (new geometry changes pointer capture).
    pub main_wake: RenderWakeNotifier,
}

/// The sections every HUD shows, in display order.
pub(crate) fn default_sections(deps: &ControlPanelDeps) -> Vec<Box<dyn Section>> {
    vec![
        Box::new(sections::AgentsSection::new(deps.pause.clone())),
        Box::new(sections::VisibilitySection::new(
            Arc::clone(&deps.scene),
            deps.render_wake.clone(),
        )),
        Box::new(sections::InputSection::new(
            deps.chords,
            deps.pairing.is_some(),
        )),
        Box::new(sections::SystemSection),
    ]
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
            events: tx,
            render_wake: deps.render_wake.clone(),
            main_wake: deps.main_wake.clone(),
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
        }
    };
    let recover = handle.clone();
    runtime.spawn(async move {
        let mut shutdown_rx = shutdown.subscribe();
        let mut task = Some(fresh());
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
                _ = shutdown_rx.recv() => break,
            }
        }
    });
    handle
}

struct PanelTask {
    handle: ControlPanelHandle,
    deps: ControlPanelDeps,
    sections: Arc<[Box<dyn Section>]>,
    open: Option<Open>,
    nav: Nav,
    published: Option<ControlPanelModel>,
}

impl PanelTask {
    async fn handle(&mut self, event: PanelEvent) {
        let effects = match (event, self.open.as_mut()) {
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
                        state::on_text(open, &text);
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
                    PanelEvent::Chord(_) => Vec::new(),
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
    }

    async fn on_chord(&mut self, role: ChordRole) {
        let paused = self.deps.pause.is_paused();
        let outcome = state::on_chord(self.open.as_ref().map(|o| o.opened_by), paused, role);
        tracing::info!(chord = role.config_key(), ?outcome, "control panel: chord");
        if let Some(pause) = outcome.pause {
            self.deps.pause.set(pause).await;
        }
        match outcome.panel {
            PanelChange::Open(by) => self.open = Some(Open::new(by)),
            PanelChange::Close => self.open = None,
            PanelChange::Keep => {}
        }
        if outcome.pairing
            && let Some(pairing) = &self.deps.pairing
        {
            pairing.open();
        }
    }

    /// Close the panel; closing always resumes agents.
    async fn close(&mut self) {
        self.open = None;
        self.deps.pause.set(false).await;
    }

    async fn apply(&mut self, row: &str, edit: Edit) {
        let section_id = row.split('.').next().unwrap_or_default();
        let Some(index) = self.sections.iter().position(|s| s.id() == section_id) else {
            tracing::warn!(row, "control panel: no section owns this row");
            return;
        };
        // Isolated: a section that panics fails its row, not the panel.
        let (sections, owned_row) = (Arc::clone(&self.sections), row.to_owned());
        let result = tokio::spawn(async move { sections[index].apply(&owned_row, edit).await })
            .await
            .unwrap_or_else(|error| {
                tracing::error!(row, %error, "control panel: section apply failed");
                Err("This setting failed; see the HUD log".to_owned())
            });
        match result {
            Ok(()) => tracing::info!(row, ?edit, "control panel: applied"),
            Err(message) => {
                tracing::warn!(row, ?edit, %message, "control panel: edit failed");
                if let Some(open) = self.open.as_mut() {
                    open.error = Some((row.to_owned(), message));
                }
            }
        }
    }

    /// Rebuild the model from the sections and publish it if it changed.
    async fn rebuild(&mut self) {
        let model = match self.open.as_mut() {
            None => None,
            Some(open) => {
                let mut rows = Vec::with_capacity(self.sections.len());
                for (index, section) in self.sections.iter().enumerate() {
                    // Isolated: a section that panics shows one error row
                    // and the rest of the panel (and any pause) stands.
                    let sections = Arc::clone(&self.sections);
                    let section_rows = tokio::spawn(async move { sections[index].rows().await })
                        .await
                        .unwrap_or_else(|error| {
                            tracing::error!(
                                section = section.id(),
                                %error,
                                "control panel: section rows failed"
                            );
                            vec![
                                Row::new(
                                    format!("{}.unavailable", section.id()),
                                    "This section failed to load",
                                    row::Control::ReadOnly {
                                        text: "See the HUD log".to_owned(),
                                        mono: false,
                                    },
                                )
                                .caution(true),
                            ]
                        });
                    rows.push(SectionRows {
                        id: section.id(),
                        title: section.title(),
                        rows: section_rows,
                    });
                }
                let ctx = ViewContext {
                    paused: self.deps.pause.is_paused(),
                    pause_chord: self.deps.chords.pause.to_string(),
                };
                let (model, nav) = state::build(open, &rows, &ctx);
                self.nav = nav;
                Some(model)
            }
        };
        let closed = model.is_none();
        if model != self.published {
            self.published = model.clone();
            self.handle.publish(model);
        }
        if closed {
            let notice = self.closed_notice().await;
            self.handle.publish_closed_notice(notice);
        }
    }

    /// The sections' notices joined, plus how to reopen the panel; `None`
    /// when no section has one.
    async fn closed_notice(&self) -> Option<String> {
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
        parts.push(format!(
            "open with {}",
            reopen_chord(&panel_chord, &self.deps.chords)
        ));
        Some(parts.join(" · "))
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
