//! The winit side of the control panel (hud-vniri.2): while the panel is open
//! it owns the keyboard and the pointer over its surface. Keys, text, clicks
//! and the wheel become [`PanelEvent`]s for the panel task; nothing reaches an
//! agent. Points outside the panel pass through to the desktop. Opening takes
//! keyboard focus and closing hands it back to the window that had it.

use tze_hud_compositor::PanelHitTarget;
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

use super::WinitApp;
use super::input_dispatch::{logical_key_to_str, physical_key_to_key_code_str};
use super::keyboard::keyboard_key_identity;
use super::lifecycle::{focus_window_for_text_input, window_is_foreground};
use crate::shell::control_panel::{ControlPanelHandle, PanelEvent, PanelKey};

/// Main-thread bookkeeping for the open panel.
#[derive(Default)]
pub(super) struct PanelInputState {
    /// The panel was open at the last focus sync (open/close edge detection).
    was_open: bool,
    /// The foreground window before the panel took focus, restored on close.
    #[cfg(target_os = "windows")]
    previous_foreground: Option<isize>,
    /// Target under the left press; a release on the same target clicks.
    pressed: Option<PanelHitTarget>,
    /// Right, middle or other buttons that went down over the panel: each
    /// one's release is the panel's too, wherever it lands.
    other_pressed: Vec<winit::event::MouseButton>,
    hovered: Option<PanelHitTarget>,
}

/// The panel key for a key press, if the panel handles it (typing is
/// separate: see [`typed_text`]).
pub(super) fn panel_key(
    physical: PhysicalKey,
    logical: &Key,
    mods: ModifiersState,
) -> Option<PanelKey> {
    let (ctrl, alt, shift) = (mods.control_key(), mods.alt_key(), mods.shift_key());
    if let PhysicalKey::Code(code) = physical {
        if alt && !ctrl {
            let digit = match code {
                KeyCode::Digit1 => Some(1),
                KeyCode::Digit2 => Some(2),
                KeyCode::Digit3 => Some(3),
                KeyCode::Digit4 => Some(4),
                KeyCode::Digit5 => Some(5),
                KeyCode::Digit6 => Some(6),
                KeyCode::Digit7 => Some(7),
                KeyCode::Digit8 => Some(8),
                KeyCode::Digit9 => Some(9),
                _ => None,
            };
            if let Some(n) = digit {
                return Some(PanelKey::Section(n));
            }
            // Letters by physical position, so Alt+H is the same key on
            // every layout's QWERTY position.
            let id = physical_key_to_key_code_str(&physical);
            if let Some(letter) = id.strip_prefix("Key")
                && letter.len() == 1
            {
                return letter
                    .chars()
                    .next()
                    .map(|c| PanelKey::Alt(c.to_ascii_lowercase()));
            }
        }
        if ctrl && !alt {
            match code {
                KeyCode::Minus | KeyCode::NumpadSubtract => return Some(PanelKey::ZoomOut),
                KeyCode::Equal | KeyCode::NumpadAdd => return Some(PanelKey::ZoomIn),
                _ => {}
            }
        }
    }
    let Key::Named(named) = logical else {
        return None;
    };
    Some(match named {
        NamedKey::ArrowUp => PanelKey::Up,
        NamedKey::ArrowDown => PanelKey::Down,
        NamedKey::ArrowLeft => PanelKey::Left,
        NamedKey::ArrowRight => PanelKey::Right,
        NamedKey::Tab if shift => PanelKey::BackTab,
        NamedKey::Tab => PanelKey::Tab,
        NamedKey::Enter => PanelKey::Enter,
        NamedKey::Space if !ctrl && !alt => PanelKey::Space,
        NamedKey::Escape => PanelKey::Escape,
        NamedKey::Backspace => PanelKey::Backspace,
        NamedKey::Delete => PanelKey::Delete,
        NamedKey::Home => PanelKey::Home,
        NamedKey::End => PanelKey::End,
        NamedKey::PageUp => PanelKey::PageUp,
        NamedKey::PageDown => PanelKey::PageDown,
        _ => return None,
    })
}

/// Printable text a key press types into the search (no Ctrl/Alt chords).
pub(super) fn typed_text(logical: &Key, mods: ModifiersState) -> Option<String> {
    if mods.control_key() || mods.alt_key() || mods.super_key() {
        return None;
    }
    match logical.as_ref() {
        Key::Character(text) => Some(text.to_string()),
        _ => None,
    }
}

impl WinitApp {
    /// The panel, when it is open.
    fn open_panel(&self) -> Option<&ControlPanelHandle> {
        self.state.control_panel.as_ref().filter(|p| p.is_open())
    }

    /// Route a key event to the open panel. Returns true when the panel owns
    /// it (the caller must not dispatch it anywhere else).
    pub(super) fn panel_key_event(&mut self, event: &KeyEvent) -> bool {
        self.panel_key_input(
            event.physical_key,
            &event.logical_key,
            event.state == ElementState::Pressed,
        )
    }

    /// [`Self::panel_key_event`] from its parts (testable without a winit
    /// event).
    pub(super) fn panel_key_input(
        &mut self,
        physical: PhysicalKey,
        logical: &Key,
        pressed: bool,
    ) -> bool {
        let Some(panel) = self.open_panel().cloned() else {
            return false;
        };
        let identity = keyboard_key_identity(
            &physical_key_to_key_code_str(&physical),
            &logical_key_to_str(logical),
        );
        if !pressed {
            self.state
                .consumed_shell_shortcut_keydowns
                .remove(&identity);
            return true;
        }
        // Its release must not reach an agent if the panel closes first.
        self.state.consumed_shell_shortcut_keydowns.insert(identity);
        let mods = self.state.modifiers;
        if let Some(key) = panel_key(physical, logical, mods) {
            panel.send(PanelEvent::Key(key));
        } else if let Some(text) = typed_text(logical, mods) {
            panel.send(PanelEvent::Text(text));
        }
        true
    }

    /// IME-composed text goes to the panel's search while it is open.
    pub(super) fn panel_text(&mut self, text: &str) -> bool {
        let Some(panel) = self.open_panel() else {
            return false;
        };
        panel.send(PanelEvent::Text(text.to_owned()));
        true
    }

    fn panel_hit(&self, primary: bool) -> Option<PanelHitTarget> {
        if !primary {
            return None;
        }
        self.open_panel()?
            .hit(self.state.cursor_x, self.state.cursor_y)
    }

    /// Pointer moved to the current cursor position. Returns true when it is
    /// over the panel (the scene must not see it).
    pub(super) fn panel_pointer_moved(&mut self, primary: bool) -> bool {
        let Some(panel) = self.open_panel().cloned() else {
            return false;
        };
        let hit = self.panel_hit(primary);
        // Hover tracks rows, tiles and chips; the bare panel is no target.
        let hover = hit.clone().filter(|t| *t != PanelHitTarget::Panel);
        if hover != self.state.panel_input.hovered {
            self.state.panel_input.hovered = hover.clone();
            panel.send(PanelEvent::Hover(hover));
        }
        hit.is_some()
    }

    /// Left button edge. Returns true when the panel owns it.
    pub(super) fn panel_left_button(&mut self, pressed: bool, primary: bool) -> bool {
        let Some(panel) = self.open_panel().cloned() else {
            return false;
        };
        let hit = self.panel_hit(primary);
        if pressed {
            if hit.is_none() {
                return false;
            }
            // A click is user input, so Windows lets the panel take focus now
            // even if it refused when the panel opened.
            if let Some(window) = &self.state.window
                && !window_is_foreground(window)
            {
                focus_window_for_text_input(window);
            }
            self.state.panel_input.pressed = hit;
            return true;
        }
        // A release whose press went to the scene (a drag that ended over
        // the panel) belongs to the scene.
        let Some(pressed) = self.state.panel_input.pressed.take() else {
            return false;
        };
        if hit.as_ref() == Some(&pressed) {
            panel.send(PanelEvent::Click(pressed));
        }
        true
    }

    /// A right, middle or other button over the panel does nothing, and the
    /// scene must not see it either (no context menu for a widget under the
    /// panel), including the release of a press that began over it. Returns
    /// true when the panel owns the edge.
    pub(super) fn panel_other_button(
        &mut self,
        button: winit::event::MouseButton,
        pressed: bool,
        primary: bool,
    ) -> bool {
        let over = self.panel_hit(primary).is_some();
        let held = &mut self.state.panel_input.other_pressed;
        let was_held = held.contains(&button);
        held.retain(|b| *b != button);
        if pressed {
            if over {
                held.push(button);
            }
            return over;
        }
        was_held || over
    }

    /// Wheel over the panel scrolls it. Returns true when the panel owns it.
    pub(super) fn panel_wheel(&mut self, dy: f32, primary: bool) -> bool {
        if self.panel_hit(primary).is_none() {
            return false;
        }
        if let Some(panel) = self.open_panel() {
            panel.send(PanelEvent::Scroll(dy));
        }
        true
    }

    /// The panel window gained or lost keyboard focus.
    pub(super) fn panel_window_focus(&mut self, focused: bool) {
        if let Some(panel) = self.open_panel() {
            panel.send(PanelEvent::WindowFocus(focused));
        }
    }

    /// True while the open panel should capture the pointer at the cursor.
    pub(super) fn panel_captures_cursor(&self) -> bool {
        // While click-through, no `CursorMoved` arrives and the position is
        // refreshed from the OS in primary-window coordinates, so a
        // `cursor_window` left over from an earlier move says nothing.
        let primary = !self.state.overlay_capturing
            || match (&self.state.window, self.state.cursor_window) {
                (Some(window), Some(id)) => window.id() == id,
                _ => true,
            };
        self.panel_hit(primary).is_some()
    }

    /// On the open/close edge: take keyboard focus when the panel opens and
    /// hand it back when it closes. Runs every `about_to_wait`; idle cost is
    /// one atomic load. (Pointer capture follows new panel geometry through
    /// the main-loop wake in `set_geometry` and the hit-test refresh every
    /// full turn runs.)
    pub(super) fn sync_panel_focus(&mut self) {
        let open = self.open_panel().is_some();
        if open == self.state.panel_input.was_open {
            return;
        }
        self.state.panel_input.was_open = open;
        self.state.panel_input.pressed = None;
        self.state.panel_input.hovered = None;
        if open {
            #[cfg(target_os = "windows")]
            {
                self.state.panel_input.previous_foreground = super::lifecycle::foreground_window();
            }
            let focused = self.state.window.as_ref().is_none_or(|window| {
                focus_window_for_text_input(window);
                window_is_foreground(window)
            });
            if !focused {
                tracing::warn!(
                    "control panel: Windows refused keyboard focus; click the panel to type"
                );
            }
            self.panel_window_focus(focused);
        } else {
            #[cfg(target_os = "windows")]
            if let Some(previous) = self.state.panel_input.previous_foreground.take() {
                super::lifecycle::restore_foreground(previous, self.state.window.as_deref());
            }
        }
        self.update_overlay_cursor_hittest();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, logical: Key, mods: ModifiersState) -> Option<PanelKey> {
        panel_key(PhysicalKey::Code(code), &logical, mods)
    }

    /// Navigation keys, Alt+digit/letter accelerators and Ctrl+−/= map to
    /// panel keys; plain letters are typing, not keys.
    #[test]
    fn keys_map_to_panel_keys() {
        let none = ModifiersState::empty();
        let named = |n| Key::Named(n);
        assert_eq!(
            key(KeyCode::ArrowDown, named(NamedKey::ArrowDown), none),
            Some(PanelKey::Down)
        );
        assert_eq!(
            key(KeyCode::Tab, named(NamedKey::Tab), ModifiersState::SHIFT),
            Some(PanelKey::BackTab)
        );
        assert_eq!(
            key(KeyCode::Escape, named(NamedKey::Escape), none),
            Some(PanelKey::Escape)
        );
        assert_eq!(
            key(
                KeyCode::Digit3,
                Key::Character("3".into()),
                ModifiersState::ALT
            ),
            Some(PanelKey::Section(3))
        );
        assert_eq!(
            key(
                KeyCode::KeyH,
                Key::Character("h".into()),
                ModifiersState::ALT
            ),
            Some(PanelKey::Alt('h'))
        );
        assert_eq!(
            key(
                KeyCode::Equal,
                Key::Character("=".into()),
                ModifiersState::CONTROL
            ),
            Some(PanelKey::ZoomIn)
        );
        assert_eq!(
            key(
                KeyCode::Minus,
                Key::Character("-".into()),
                ModifiersState::CONTROL
            ),
            Some(PanelKey::ZoomOut)
        );
        assert_eq!(key(KeyCode::KeyH, Key::Character("h".into()), none), None);
        assert_eq!(
            key(
                KeyCode::Space,
                named(NamedKey::Space),
                ModifiersState::CONTROL
            ),
            None
        );
    }

    #[test]
    fn typed_text_skips_modifier_chords() {
        let h = Key::Character("h".into());
        assert_eq!(
            typed_text(&h, ModifiersState::empty()).as_deref(),
            Some("h")
        );
        assert_eq!(typed_text(&h, ModifiersState::SHIFT).as_deref(), Some("h"));
        assert_eq!(typed_text(&h, ModifiersState::CONTROL), None);
        assert_eq!(typed_text(&h, ModifiersState::ALT), None);
        assert_eq!(
            typed_text(&Key::Named(NamedKey::Enter), ModifiersState::empty()),
            None
        );
    }
}

#[cfg(test)]
mod panel_task_tests {
    use crate::shell::control_panel::{
        ControlPanelDeps, ControlPanelHandle, PanelEvent, PanelKey, PauseControl, default_sections,
        spawn_control_panel,
    };
    use crate::windowed::WindowedRuntimeState;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tze_hud_compositor::{ControlPanelModel, PanelHitTarget};
    use tze_hud_compositor::{PanelBody, PanelFocus, PanelPart, PanelTone};
    use tze_hud_scene::config::{ChordRole, HudChords};
    use tze_hud_scene::render_wake::RenderWakeNotifier;
    use tze_hud_scene::types::LeaseState;

    struct Fixture {
        state: WindowedRuntimeState,
        panel: ControlPanelHandle,
        wakes: Arc<AtomicUsize>,
    }

    fn fixture() -> Fixture {
        fixture_with(default_sections)
    }

    fn fixture_with(
        sections: impl FnOnce(&ControlPanelDeps) -> Vec<Box<dyn crate::shell::control_panel::Section>>,
    ) -> Fixture {
        fixture_with_clock(sections, Arc::new(tze_hud_scene::SystemClock::new()))
    }

    fn fixture_with_clock(
        sections: impl FnOnce(&ControlPanelDeps) -> Vec<Box<dyn crate::shell::control_panel::Section>>,
        clock: Arc<dyn tze_hud_scene::Clock>,
    ) -> Fixture {
        let state = WindowedRuntimeState::new_headless_with_clock(clock);
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&wakes);
        let render_wake = RenderWakeNotifier::new(move || {
            w.fetch_add(1, Ordering::SeqCst);
        });
        let deps = ControlPanelDeps {
            scene: Arc::clone(&state.shared_state.try_lock().expect("fresh state").scene),
            pause: PauseControl::new(
                Arc::clone(&state.shared_state),
                Arc::clone(&state.chrome_state),
                Arc::clone(&state.safe_mode_atomic),
                render_wake.clone(),
                Some(state.portal_projection_driver.portal_handle()),
                state.wake.main_work_notifier(),
            ),
            chords: crate::shell::control_panel::chords::ChordControl::fixed(HudChords::default()),
            pairing: None,
            agents: None,
            connect: None,
            runtime: None,
            appearance: None,
            render_wake,
            main_wake: Default::default(),
        };
        let sections = sections(&deps);
        let panel = spawn_control_panel(
            &tokio::runtime::Handle::current(),
            deps,
            sections,
            state.shutdown.clone(),
        );
        Fixture {
            state,
            panel,
            wakes,
        }
    }

    impl Fixture {
        fn paused(&self) -> bool {
            self.state.safe_mode_atomic.load(Ordering::Acquire)
        }

        /// Send `event` and wait until the task has handled it (a Refresh
        /// round trip republishes nothing when the model is unchanged, so
        /// poll the observable state instead).
        async fn send_and_wait(&self, event: PanelEvent, done: impl Fn(&Self) -> bool) {
            assert!(self.panel.send(event));
            for _ in 0..200 {
                if done(self) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("panel never reached the expected state");
        }

        fn model(&self) -> Option<ControlPanelModel> {
            self.panel.current_model()
        }
    }

    /// The pause chord pauses agents (suspending leases) and opens the panel;
    /// pressing it again resumes and closes. Drives the production task
    /// against the headless runtime's real scene and safe-mode flag.
    #[tokio::test(flavor = "multi_thread")]
    async fn portal_liveness_paused_during_safe_mode() {
        use tze_hud_projection::hub::{PortalHub, PortalKey, PortalLimits, Publish};
        use tze_hud_scene::{Clock, TestClock};

        // Wall time can rewind independently; monotonic observations never do.
        #[derive(Debug)]
        struct DivergentClock {
            wall: TestClock,
            mono: std::sync::atomic::AtomicU64,
        }
        impl Clock for DivergentClock {
            fn now_us(&self) -> u64 {
                self.wall.now_us()
            }
            fn monotonic_us(&self) -> u64 {
                self.mono.fetch_add(1, Ordering::Relaxed)
            }
        }
        let clock = Arc::new(DivergentClock {
            wall: TestClock::new(1_000),
            mono: std::sync::atomic::AtomicU64::new(9_000_000_000_000),
        });
        assert!(clock.monotonic_us() > clock.now_us());
        let mut f = fixture_with_clock(default_sections, clock.clone());
        let lease_id = f
            .state
            .shared_state
            .lock()
            .await
            .scene
            .lock()
            .await
            .grant_lease("agent", 60_000);
        let lease_state = || async {
            let st = f.state.shared_state.lock().await;
            let scene = st.scene.lock().await;
            scene.leases[&lease_id].state
        };

        f.send_and_wait(PanelEvent::Chord(ChordRole::Pause), |f| {
            f.paused() && f.panel.is_open()
        })
        .await;
        assert_eq!(lease_state().await, LeaseState::Suspended);
        assert!(f.state.chrome_state.read().unwrap().safe_mode_active);
        let model = f.model().expect("panel open");
        assert_eq!(model.tiles[1].id, "agents.pause");
        assert_eq!(
            model.tiles[1].emphasis,
            PanelTone::Caution,
            "paused tile is caution"
        );
        assert_eq!(model.hint_end, "Esc close and resume");

        f.send_and_wait(PanelEvent::Chord(ChordRole::Pause), |f| {
            !f.paused() && !f.panel.is_open()
        })
        .await;
        assert_eq!(lease_state().await, LeaseState::Active);
        assert!(f.model().is_none());

        let portals = f.state.portal_projection_driver.portal_handle();
        let scene_handle = Arc::clone(&f.state.shared_state.lock().await.scene);
        assert!(Arc::ptr_eq(
            &portals.clock,
            &scene_handle.lock().await.clock()
        ));
        *portals.hub.lock().unwrap() = PortalHub::new(PortalLimits {
            input_ttl_us: 80_000_000,
            ..Default::default()
        });
        let mcp = tze_hud_mcp::McpServer::with_shared_scene(Arc::clone(&scene_handle))
            .with_config(tze_hud_mcp::McpConfig::with_psk("panel-test"))
            .with_portals(portals.clone())
            .with_safe_mode(Arc::clone(&f.state.safe_mode_atomic));
        async fn call(
            mcp: &tze_hud_mcp::McpServer,
            name: &str,
            arguments: serde_json::Value,
        ) -> serde_json::Value {
            let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}).to_string();
            serde_json::from_str(
                &mcp.dispatch(&body, &mcp.caller_context(Some("panel-test".into())))
                    .await,
            )
            .unwrap()
        }
        assert_eq!(
            call(
                &mcp,
                "hud_publish",
                serde_json::json!({"surface":"portal:mcp", "content":"before"})
            )
            .await["result"]["isError"],
            serde_json::Value::Null
        );
        // Separate agent identities let one genuine poll refresh exactly one row.
        let key = |id: &str| PortalKey::new(id, id);
        let publish = |id: &str| {
            portals
                .hub
                .lock()
                .unwrap()
                .publish_with_hold(
                    &key(id),
                    Publish {
                        text: Some(id.into()),
                        ..Default::default()
                    },
                    None,
                    clock.now_us(),
                )
                .unwrap()
        };
        for id in [
            "idle",
            "finite",
            "indefinite",
            "polled",
            "cancelled",
            "orphan",
            "recovered",
            "replace",
            "dismiss",
            "input",
            "max",
        ] {
            publish(id);
        }
        {
            let mut hub = portals.hub.lock().unwrap();
            hub.hold(&key("finite"), 50_000, clock.now_us()).unwrap();
            hub.hold(&key("indefinite"), 0, clock.now_us()).unwrap();
            hub.hold(&key("max"), 0, clock.now_us()).unwrap();
            hub.submit_reply(&key("input"), "expires while paused".into(), clock.now_us())
                .unwrap();
        }
        let (tiles, leases, tab) = {
            let mut scene = scene_handle.lock().await;
            let tab = scene.create_tab("Portals", 1).unwrap();
            f.state.portal_projection_driver.drain(
                &mut scene,
                &mut f.state.input_processor,
                Some(tab),
            );
            let tiles: std::collections::BTreeMap<_, _> = scene
                .tiles
                .values()
                .filter_map(|tile| {
                    let identity = &scene
                        .overlay
                        .portal_surfaces
                        .get(&tile.id)?
                        .identity
                        .session_id;
                    Some((identity.rsplit('/').next()?.to_owned(), tile.id))
                })
                .collect();
            let leases: std::collections::BTreeMap<_, _> = tiles
                .iter()
                .map(|(id, tile)| (id.clone(), scene.tiles[tile].lease_id))
                .collect();
            (tiles, leases, tab)
        };
        clock.wall.advance(10_000);
        // Accepted but unpainted before pause; the driver must not grant around it.
        publish("unrendered");
        for id in ["orphan", "recovered"] {
            portals
                .hub
                .lock()
                .unwrap()
                .degrade(&key(id), clock.now_us());
            scene_handle
                .lock()
                .await
                .disconnect_lease(&leases[id], clock.now_millis())
                .unwrap();
        }
        let input_expiry = portals
            .hub
            .lock()
            .unwrap()
            .get(&key("input"))
            .unwrap()
            .inputs[0]
            .expires_us;
        f.send_and_wait(PanelEvent::Chord(ChordRole::Pause), |f| f.paused())
            .await;
        let pause = PauseControl::new(
            Arc::clone(&f.state.shared_state),
            Arc::clone(&f.state.chrome_state),
            Arc::clone(&f.state.safe_mode_atomic),
            Default::default(),
            Some(portals.clone()),
            f.state.wake.main_work_notifier(),
        );
        pause.set(true).await;
        let refused = call(
            &mcp,
            "hud_publish",
            serde_json::json!({"surface":"portal:mcp", "content":"rejected"}),
        )
        .await;
        assert_eq!(refused["result"]["isError"], true);
        assert!(refused.to_string().contains("SAFE_MODE_ACTIVE"));
        clock.wall.advance(20_000);
        {
            let mut hub = portals.hub.lock().unwrap();
            hub.poll_input("polled", &[], Some(0), clock.now_us());
            hub.poll_input("recovered", &[], Some(0), clock.now_us());
            let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let request = hub
                .begin_input_poll("cancelled", 5_000_000, clock.now_us(), Arc::clone(&active))
                .unwrap()
                .unwrap();
            active.store(false, Ordering::Release);
            hub.end_input_poll("cancelled", request);
            hub.clear(&key("replace")).unwrap();
        }
        {
            let mut scene = scene_handle.lock().await;
            f.state
                .portal_projection_driver
                .viewer_dismiss_tile(&mut scene, tiles["dismiss"]);
        }
        clock.wall.advance(80_000);
        let resume_wall = clock.now_us();
        {
            let mut scene = scene_handle.lock().await;
            scene.expire_leases();
            let version = scene.version;
            f.state.portal_projection_driver.drain(
                &mut scene,
                &mut f.state.input_processor,
                Some(tab),
            );
            assert_eq!(scene.leases[&leases["idle"]].state, LeaseState::Suspended);
            assert!(!scene.tiles.contains_key(&tiles["orphan"]));
            assert!(
                !scene.tiles.contains_key(&tiles["recovered"]),
                "poll recovery cannot extend pre-existing scene grace"
            );
            assert!(!scene.tiles.contains_key(&tiles["replace"]));
            assert!(!scene.tiles.contains_key(&tiles["dismiss"]));
            assert_eq!(scene.tiles.len(), tiles.len() - 4);
            // A second idle paused drain neither consumes readiness nor mutates pixels.
            f.state.portal_projection_driver.drain(
                &mut scene,
                &mut f.state.input_processor,
                Some(tab),
            );
            assert!(scene.version >= version);
            let settled_version = scene.version;
            f.state.portal_projection_driver.drain(
                &mut scene,
                &mut f.state.input_processor,
                Some(tab),
            );
            assert_eq!(scene.version, settled_version);
            let hub = portals.hub.lock().unwrap();
            assert!(hub.get(&key("unrendered")).unwrap().dirty);
            assert!(hub.get(&key("input")).unwrap().inputs.is_empty());
            assert_eq!(hub.get(&key("idle")).unwrap().transcript[0].text, "idle");
            assert!(
                !scene
                    .overlay
                    .portal_surfaces
                    .values()
                    .any(|surface| surface.identity.session_id.ends_with("/unrendered"))
            );
            drop(hub);
            let deadline = f
                .state
                .portal_projection_driver
                .next_wake_deadline(&scene)
                .unwrap();
            assert!(
                deadline.wall_us > resume_wall,
                "blocked render/cue debt cannot spin on a past deadline"
            );
        }
        let readiness = f.state.wake.main_work_generation();
        f.send_and_wait(PanelEvent::Chord(ChordRole::Pause), |f| {
            !f.paused() && !f.panel.is_open()
        })
        .await;
        assert_eq!(f.state.wake.main_work_generation(), readiness + 1);
        let readiness = f.state.wake.main_work_generation();
        pause.set(false).await;
        assert_eq!(
            f.state.wake.main_work_generation(),
            readiness,
            "idempotent exit adds no readiness"
        );
        assert!(
            !publish("replace").reattached,
            "explicit clear starts a fresh attach"
        );
        assert!(
            publish("dismiss").reattached,
            "runtime dismiss retains a tombstone"
        );
        {
            let mut scene = scene_handle.lock().await;
            f.state.portal_projection_driver.drain(
                &mut scene,
                &mut f.state.input_processor,
                Some(tab),
            );
            assert_eq!(scene.tiles[&tiles["idle"]].lease_id, leases["idle"]);
            assert_eq!(scene.leases[&leases["idle"]].state, LeaseState::Active);
            assert_eq!(
                portals
                    .hub
                    .lock()
                    .unwrap()
                    .get(&key("unrendered"))
                    .unwrap()
                    .unread,
                0
            );
        }
        {
            let mut hub = portals.hub.lock().unwrap();
            assert!(hub.sweep(resume_wall + 19_999_999).is_empty());
            hub.sweep(resume_wall + 20_000_000);
            for id in ["idle", "cancelled"] {
                assert!(hub.get(&key(id)).unwrap().degraded_since_us.is_some());
            }
            assert!(hub.get(&key("polled")).unwrap().degraded_since_us.is_none());
            hub.sweep(resume_wall + 30_000_000);
            assert!(
                hub.get(&key("polled")).unwrap().degraded_since_us.is_some(),
                "a genuine paused poll gets 30 s, without credit for the remaining pause"
            );
            assert!(hub.get(&key("finite")).unwrap().degraded_since_us.is_none());
            hub.sweep(resume_wall + 40_000_000);
            assert!(hub.get(&key("finite")).unwrap().degraded_since_us.is_some());
            assert!(
                hub.get(&key("indefinite"))
                    .unwrap()
                    .degraded_since_us
                    .is_none()
            );
            assert_eq!(input_expiry, 81_000_000, "input TTL was never shifted");
        }
        // Maximum suspension still reaps a healthy scene surface and tombstones
        // its hub attachment. Resume must not grant it a replacement lease.
        pause.set(true).await;
        clock
            .wall
            .advance(tze_hud_scene::SceneGraph::DEFAULT_MAX_SUSPENSION_MS + 1);
        {
            let mut scene = scene_handle.lock().await;
            scene.expire_leases();
            f.state.portal_projection_driver.drain(
                &mut scene,
                &mut f.state.input_processor,
                Some(tab),
            );
            assert!(portals.hub.lock().unwrap().get(&key("max")).is_none());
        }
        pause.set(false).await;
        {
            let mut scene = scene_handle.lock().await;
            f.state.portal_projection_driver.drain(
                &mut scene,
                &mut f.state.input_processor,
                Some(tab),
            );
            assert!(!scene.tiles.contains_key(&tiles["max"]));
        }
        // Rewind wall time independently of monotonically increasing readings;
        // saturating deadlines near u64::MAX must never wrap into the past.
        for (start, paused_at, resumed_at) in [
            (100_000_000, 110_000_000, 50_000_000),
            (
                u64::MAX - 40_000_000,
                u64::MAX - 30_000_000,
                u64::MAX - 10_000_000,
            ),
        ] {
            clock.wall.set_us(start);
            *portals.hub.lock().unwrap() = PortalHub::new(PortalLimits::default());
            *scene_handle.lock().await =
                tze_hud_scene::SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
            publish("remaining");
            clock.wall.set_us(paused_at);
            pause.set(true).await;
            pause.set(true).await;
            clock.wall.set_us(resumed_at);
            pause.set(false).await;
            pause.set(false).await;
            let deadline = resumed_at.saturating_add(20_000_000);
            let mut hub = portals.hub.lock().unwrap();
            hub.take_due(resumed_at);
            assert_eq!(hub.next_deadline().unwrap().at_us, deadline);
            assert!(hub.sweep(deadline - 1).is_empty());
            hub.sweep(deadline);
            assert!(
                hub.get(&key("remaining"))
                    .unwrap()
                    .degraded_since_us
                    .is_some()
            );
        }
        // Poison refuses agent work but never vetoes local pause/resume.
        let hub = Arc::clone(&portals.hub);
        let _ = std::thread::spawn(move || {
            let _guard = hub.lock().unwrap();
            panic!("poison fixture");
        })
        .join();
        pause.set(true).await;
        assert!(f.paused());
        assert_eq!(
            f.state
                .portal_projection_driver
                .next_wake_deadline(&*scene_handle.lock().await),
            None
        );
        pause.set(false).await;
        assert!(!f.paused());
    }

    /// The panel chord opens without pausing; the pause tile pauses in place;
    /// Esc closes and resumes.
    #[tokio::test(flavor = "multi_thread")]
    async fn panel_chord_opens_running_and_escape_closes_and_resumes() {
        let f = fixture();
        f.send_and_wait(PanelEvent::Chord(ChordRole::Panel), |f| f.panel.is_open())
            .await;
        assert!(!f.paused());

        let tile = PanelHitTarget::Tile(1, PanelPart::Body);
        f.send_and_wait(PanelEvent::Click(tile), |f| f.paused())
            .await;
        assert!(f.panel.is_open(), "pausing from the panel keeps it open");
        assert_eq!(f.model().unwrap().focus, PanelFocus::Tile(1));

        // The pause chord on a panel opened by the panel chord only toggles.
        f.send_and_wait(PanelEvent::Chord(ChordRole::Pause), |f| !f.paused())
            .await;
        assert!(f.panel.is_open());
        f.send_and_wait(PanelEvent::Chord(ChordRole::Pause), |f| f.paused())
            .await;

        f.send_and_wait(PanelEvent::Key(PanelKey::Escape), |f| !f.panel.is_open())
            .await;
        assert!(!f.paused(), "closing always resumes");
    }

    /// Visibility (CP-6): Alt+H hides everything, a class row shows one class
    /// again, and closing leaves a notice naming what is hidden and the chord
    /// that reopens the panel; Hide all off shows everything and clears it.
    /// The scene's leases stay untouched throughout.
    #[tokio::test(flavor = "multi_thread")]
    async fn hide_all_class_rows_and_the_closed_notice() {
        use tze_hud_scene::graph::SurfaceClasses;

        let f = fixture();
        let scene = Arc::clone(&f.state.shared_state.lock().await.scene);
        let lease_id = scene.lock().await.grant_lease("agent", 60_000);
        let hidden = || scene.try_lock().map(|s| s.owner_hidden()).ok();
        let row = |id: &str| {
            let Some(PanelBody::Sections(cards)) = f.model().map(|m| m.body) else {
                return None;
            };
            cards.into_iter().flat_map(|c| c.items).find(|i| i.id == id)
        };

        f.send_and_wait(PanelEvent::Chord(ChordRole::Panel), |f| f.panel.is_open())
            .await;
        let model = f.model().unwrap();
        assert_eq!(model.tiles[0].id, "visibility.hide_all");
        assert_eq!(model.tiles[0].shortcut.as_deref(), Some("Alt+H"));

        let wakes = f.wakes.load(Ordering::SeqCst);
        f.send_and_wait(PanelEvent::Key(PanelKey::Alt('h')), |_| {
            hidden() == Some(SurfaceClasses::ALL)
        })
        .await;
        assert!(f.wakes.load(Ordering::SeqCst) > wakes, "hiding repaints");
        f.send_and_wait(PanelEvent::Hover(None), |f| {
            f.model()
                .is_some_and(|m| m.tiles[0].emphasis == PanelTone::Caution)
        })
        .await;
        assert!(
            row("visibility.portals").is_some_and(|r| r.emphasis == PanelTone::Caution
                && r.control == tze_hud_compositor::PanelControl::Toggle(false))
        );

        // Show portals again from its row: every other class stays hidden.
        f.send_and_wait(
            PanelEvent::Click(PanelHitTarget::Row(
                "visibility.portals".into(),
                PanelPart::Body,
            )),
            |_| hidden().is_some_and(|h| !h.portals && h.zones),
        )
        .await;

        assert!(
            f.panel.current_closed_notice().is_none(),
            "no notice while open"
        );
        f.send_and_wait(PanelEvent::Key(PanelKey::Escape), |f| !f.panel.is_open())
            .await;
        f.send_and_wait(PanelEvent::Hover(None), |f| {
            f.panel.current_closed_notice().is_some()
        })
        .await;
        assert_eq!(
            f.panel.current_closed_notice(),
            Some(tze_hud_compositor::PanelNotice {
                text: "Zones and widgets hidden".into(),
                key: "Ctrl+Shift+F11".into(),
            })
        );

        // Hide all off (it is not all hidden, so the toggle first hides all).
        f.send_and_wait(PanelEvent::Chord(ChordRole::Panel), |f| f.panel.is_open())
            .await;
        f.send_and_wait(PanelEvent::Key(PanelKey::Alt('h')), |_| {
            hidden() == Some(SurfaceClasses::ALL)
        })
        .await;
        f.send_and_wait(PanelEvent::Key(PanelKey::Alt('h')), |_| {
            hidden() == Some(SurfaceClasses::NONE)
        })
        .await;
        f.send_and_wait(PanelEvent::Chord(ChordRole::Panel), |f| {
            !f.panel.is_open() && f.panel.current_closed_notice().is_none()
        })
        .await;
        // Hide all, then show every surviving class individually. The internal
        // tile bit remains set, but no visible class is suppressed.
        f.send_and_wait(PanelEvent::Chord(ChordRole::Panel), |f| f.panel.is_open())
            .await;
        f.send_and_wait(PanelEvent::Key(PanelKey::Alt('h')), |_| {
            hidden() == Some(SurfaceClasses::ALL)
        })
        .await;
        f.send_and_wait(PanelEvent::Hover(None), |_| {
            row("visibility.hide_all")
                .is_some_and(|r| r.control == tze_hud_compositor::PanelControl::Toggle(true))
        })
        .await;
        for id in [
            "visibility.portals",
            "visibility.zones",
            "visibility.widgets",
        ] {
            f.send_and_wait(
                PanelEvent::Click(PanelHitTarget::Row(id.into(), PanelPart::Body)),
                |_| {
                    row(id).is_some_and(|r| {
                        r.control == tze_hud_compositor::PanelControl::Toggle(true)
                    })
                },
            )
            .await;
        }
        assert_eq!(
            hidden(),
            Some(SurfaceClasses {
                tiles: true,
                ..SurfaceClasses::NONE
            })
        );
        f.send_and_wait(PanelEvent::Hover(None), |_| {
            row("visibility.hide_all").is_some_and(|r| {
                r.emphasis != PanelTone::Caution
                    && !r.reset
                    && r.detail.as_deref() == Some("Hides every agent surface; agents keep running")
            })
        })
        .await;
        let hide_all = row("visibility.hide_all").expect("published visibility row");
        assert_eq!(
            hide_all.control,
            tze_hud_compositor::PanelControl::Toggle(false)
        );
        assert_ne!(hide_all.emphasis, PanelTone::Caution);
        assert!(!hide_all.reset);
        f.send_and_wait(PanelEvent::Chord(ChordRole::Panel), |f| !f.panel.is_open())
            .await;
        assert!(f.panel.current_closed_notice().is_none());
        // reset_shows_what_is_hidden also awaits Section::closed_notice directly
        // under this same mask, proving None after computation completes.
        assert_eq!(
            hidden(),
            Some(SurfaceClasses {
                tiles: true,
                ..SurfaceClasses::NONE
            })
        );
        assert_eq!(
            scene.lock().await.leases[&lease_id].state,
            LeaseState::Active,
            "hiding never touches leases"
        );
    }

    /// Typing searches; input after close is ignored; input that changes
    /// nothing republishes nothing (no wake, so no frame).
    #[tokio::test(flavor = "multi_thread")]
    async fn search_idle_cost_and_stale_input() {
        let f = fixture();
        f.send_and_wait(PanelEvent::Chord(ChordRole::Panel), |f| f.panel.is_open())
            .await;
        f.send_and_wait(PanelEvent::Text("chord".into()), |f| {
            matches!(f.model().map(|m| m.body), Some(PanelBody::Results { .. }))
        })
        .await;
        let Some(PanelBody::Results { items, .. }) = f.model().map(|m| m.body) else {
            unreachable!()
        };
        let ids: Vec<_> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            ["input.pause_chord", "input.panel_chord"],
            "pairing is off in this fixture, so its chord is not listed"
        );

        // Events are handled in order, so once the sentinel's change shows,
        // the five no-op hovers before it have been handled too.
        let wakes = f.wakes.load(Ordering::SeqCst);
        for _ in 0..5 {
            assert!(f.panel.send(PanelEvent::Hover(None)));
        }
        f.send_and_wait(PanelEvent::Text("s".into()), |f| {
            f.model().is_some_and(|m| m.query == "chords")
        })
        .await;
        assert_eq!(
            f.wakes.load(Ordering::SeqCst),
            wakes + 1,
            "only the sentinel changed the model, so only it woke the compositor"
        );

        f.send_and_wait(PanelEvent::Chord(ChordRole::Panel), |f| !f.panel.is_open())
            .await;
        assert!(f.panel.send(PanelEvent::Key(PanelKey::Down)));
        assert!(f.panel.send(PanelEvent::Text("x".into())));
        f.send_and_wait(PanelEvent::Chord(ChordRole::Panel), |f| f.panel.is_open())
            .await;
        let model = f.model().unwrap();
        assert_eq!(model.query, "", "text sent while closed was dropped");
        assert_eq!(
            model.focus,
            PanelFocus::Search,
            "keys sent while closed were dropped"
        );
    }

    /// When Windows refuses the panel keyboard focus the header says so, and
    /// the chords still close it and resume (CP-1).
    #[tokio::test(flavor = "multi_thread")]
    async fn refused_focus_shows_a_notice_and_chords_still_work() {
        let f = fixture();
        f.send_and_wait(PanelEvent::Chord(ChordRole::Pause), |f| {
            f.panel.is_open() && f.paused()
        })
        .await;
        assert_eq!(f.model().unwrap().notice, None);
        f.send_and_wait(PanelEvent::WindowFocus(false), |f| {
            f.model().is_some_and(|m| m.notice.is_some())
        })
        .await;
        assert_eq!(
            f.model().unwrap().notice.as_deref(),
            Some("Click the panel to type · chords still work")
        );
        f.send_and_wait(PanelEvent::WindowFocus(true), |f| {
            f.model().is_some_and(|m| m.notice.is_none())
        })
        .await;
        f.send_and_wait(PanelEvent::WindowFocus(false), |f| {
            f.model().is_some_and(|m| m.notice.is_some())
        })
        .await;
        f.send_and_wait(PanelEvent::Chord(ChordRole::Pause), |f| {
            !f.panel.is_open() && !f.paused()
        })
        .await;
    }

    /// An open, idle panel re-reads its sections once a second but wakes the
    /// compositor only when something changed: several refresh ticks with
    /// nothing new draw nothing (CP-11). This fixture has the always-present
    /// sections only (no agents, runtime or appearance services), so it
    /// proves the refresh loop and the publish dedupe, not that every
    /// section's rows are stable between ticks.
    #[tokio::test(start_paused = true)]
    async fn an_idle_open_panel_does_not_wake_on_refresh_ticks() {
        let f = fixture();
        f.send_and_wait(PanelEvent::Chord(ChordRole::Panel), |f| f.panel.is_open())
            .await;
        // Let the first ticks and the opening rebuild settle.
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let wakes = f.wakes.load(Ordering::SeqCst);
        // Paused time: this sleep runs five refresh intervals.
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(
            f.wakes.load(Ordering::SeqCst),
            wakes,
            "refresh ticks with nothing new wake nothing"
        );
        // The panel is still live: a real change still wakes it.
        f.send_and_wait(PanelEvent::Text("p".into()), |f| {
            f.model().is_some_and(|m| m.query == "p")
        })
        .await;
        assert!(f.wakes.load(Ordering::SeqCst) > wakes);
    }

    /// A theme preview slower than the panel waits (a heavy restyle): the
    /// steps, Enter and Esc sent while it runs queue and apply in order,
    /// never dropped as a second edit of a busy row.
    #[tokio::test(flavor = "multi_thread")]
    async fn edits_behind_a_slow_preview_apply_in_order() {
        use crate::shell::control_panel::row::{BoxFuture, Control, Edit, Row, Section};
        use std::sync::Mutex;

        #[derive(Clone, Default)]
        struct SlowTheme(Arc<Mutex<Vec<Edit>>>);
        impl Section for SlowTheme {
            fn id(&self) -> &'static str {
                "look"
            }
            fn title(&self) -> &'static str {
                "Look"
            }
            fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
                Box::pin(async {
                    vec![
                        Row::new(
                            "look.theme",
                            "Theme",
                            Control::Choice {
                                options: vec!["a".into(), "b".into()],
                                selected: 0,
                            },
                        )
                        .previews(),
                    ]
                })
            }
            fn apply<'a>(&'a self, _: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
                Box::pin(async move {
                    if matches!(edit, Edit::Preview(_)) {
                        tokio::time::sleep(Duration::from_millis(400)).await;
                    }
                    self.0.lock().unwrap().push(edit);
                    Ok(())
                })
            }
        }
        let theme = SlowTheme::default();
        let edits = Arc::clone(&theme.0);
        let f = fixture_with(move |_| vec![Box::new(theme)]);
        f.send_and_wait(PanelEvent::Chord(ChordRole::Panel), |f| f.panel.is_open())
            .await;
        f.send_and_wait(PanelEvent::Key(PanelKey::Down), |f| {
            f.model()
                .is_some_and(|m| m.focus == PanelFocus::Row("look.theme".into()))
        })
        .await;
        let settled = |n: usize| {
            let edits = Arc::clone(&edits);
            async move {
                for _ in 0..300 {
                    if edits.lock().unwrap().len() >= n {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        };
        assert!(f.panel.send(PanelEvent::Key(PanelKey::Right)));
        // Esc while the preview still runs.
        assert!(f.panel.send(PanelEvent::Key(PanelKey::Escape)));
        settled(2).await;
        assert_eq!(*edits.lock().unwrap(), [Edit::Preview(1), Edit::Revert]);
        assert!(f.panel.is_open(), "the Esc reverted; it did not close");

        // Three steps and Enter while the first still runs: the waiting
        // steps merge into one restyle (key repeat), then Enter keeps what
        // the section shows, in order.
        edits.lock().unwrap().clear();
        for key in [
            PanelKey::Right,
            PanelKey::Right,
            PanelKey::Right,
            PanelKey::Enter,
        ] {
            assert!(f.panel.send(PanelEvent::Key(key)));
        }
        settled(3).await;
        assert_eq!(
            *edits.lock().unwrap(),
            [Edit::Preview(1), Edit::Preview(2), Edit::Keep]
        );

        // Steps then Esc while busy: the waiting steps are dropped.
        edits.lock().unwrap().clear();
        for key in [PanelKey::Right, PanelKey::Right, PanelKey::Escape] {
            assert!(f.panel.send(PanelEvent::Key(key)));
        }
        settled(2).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(*edits.lock().unwrap(), [Edit::Preview(1), Edit::Revert]);
    }

    /// Every panel action logs one line with the row's old and new value,
    /// whichever section owns it (G6).
    #[tokio::test]
    async fn each_action_logs_its_old_and_new_value() {
        #[derive(Clone, Default)]
        struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        // Current-thread runtime: the panel task logs on this thread.
        let _guard = tracing::subscriber::set_default(subscriber);

        let f = fixture();
        f.send_and_wait(PanelEvent::Chord(ChordRole::Panel), |f| f.panel.is_open())
            .await;
        let scene = Arc::clone(&f.state.shared_state.lock().await.scene);
        let hidden = || scene.try_lock().map(|s| s.owner_hidden()).ok();
        f.send_and_wait(
            PanelEvent::Click(PanelHitTarget::Row(
                "visibility.portals".into(),
                PanelPart::Body,
            )),
            // The log line follows the change: wait for both.
            |_| {
                hidden().is_some_and(|h| h.portals)
                    && String::from_utf8_lossy(&sink.0.lock().unwrap())
                        .contains("control panel: applied")
            },
        )
        .await;
        let log = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<_> = log
            .lines()
            .filter(|l| l.contains("control panel: applied"))
            .collect();
        assert_eq!(lines.len(), 1, "{log}");
        assert!(
            lines[0].contains(r#"row="visibility.portals""#)
                && lines[0].contains(r#"old="on""#)
                && lines[0].contains(r#"new="off""#),
            "{}",
            lines[0]
        );
    }

    /// The winit-side routing: while the panel is open it owns every key
    /// (typed text searches, Esc closes) and the pointer inside it; a click
    /// is press and release on the same target; outside the panel the
    /// pointer passes through. After close nothing is captured.
    #[tokio::test(flavor = "multi_thread")]
    async fn winit_routing_keys_and_pointer_reach_the_open_panel_only() {
        use crate::windowed::WinitApp;
        use winit::event::MouseButton::{Middle, Right};
        use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey, SmolStr};

        let f = fixture();
        let panel = f.panel.clone();
        let mut state = f.state;
        state.control_panel = Some(panel.clone());
        let mut app = WinitApp { state };
        let letter = |c: &str| {
            (
                PhysicalKey::Code(KeyCode::KeyC),
                Key::Character(SmolStr::new(c)),
            )
        };
        let (code, logical) = letter("c");
        assert!(
            !app.panel_key_input(code, &logical, true),
            "closed panel takes no keys"
        );

        panel.send(PanelEvent::Chord(ChordRole::Panel));
        wait("1", || panel.is_open()).await;

        // A typed key searches; its release is swallowed too.
        assert!(app.panel_key_input(code, &logical, true));
        assert!(app.panel_key_input(code, &logical, false));
        wait("2", || {
            panel.current_model().is_some_and(|m| m.query == "c")
        })
        .await;

        // Pointer: lay the model out the way the compositor would.
        let model = panel.current_model().unwrap();
        let geometry = tze_hud_compositor::estimate_control_panel_geometry(
            &Default::default(),
            &model,
            1920.0,
            1080.0,
        );
        panel.set_geometry(1, || geometry.clone());
        let tile = geometry
            .hits
            .iter()
            .find(|h| h.target == PanelHitTarget::Tile(1, PanelPart::Body))
            .expect("pause tile")
            .rect;
        (app.state.cursor_x, app.state.cursor_y) = (tile.x + tile.w / 2.0, tile.y + tile.h / 2.0);
        assert!(app.panel_captures_cursor());
        assert!(
            app.panel_other_button(Right, true, true),
            "a right click over the panel never reaches the scene"
        );
        assert!(
            app.panel_other_button(Right, false, false),
            "nor does its release"
        );
        assert!(
            !app.panel_other_button(Right, true, false),
            "another display's pointer is the scene's"
        );
        app.panel_other_button(Right, false, false);
        // Each button keeps its own press: a middle press over the panel
        // owns its release even after a right press elsewhere ended.
        assert!(app.panel_other_button(Middle, true, true));
        assert!(!app.panel_other_button(Right, true, false));
        assert!(!app.panel_other_button(Right, false, false));
        assert!(
            app.panel_other_button(Middle, false, false),
            "the middle release stays the panel's"
        );
        assert!(app.panel_left_button(true, true));
        assert!(app.panel_left_button(false, true));
        wait("3", || app.state.safe_mode_atomic.load(Ordering::Acquire)).await;

        // A release whose press went to the scene stays with the scene.
        assert!(!app.panel_left_button(false, true));

        // In click-through overlay mode raw mouse motion must keep waking the
        // loop while the panel is open, even with nothing interactive in the
        // scene, and entering the panel must count as a capture change.
        app.state.effective_mode = crate::windowed::WindowMode::Overlay;
        app.state.overlay_capturing = false;
        assert!(
            app.cursor_entry_armed(),
            "open panel arms cursor-entry wakes"
        );
        assert!(
            app.cursor_entry_signature().capture,
            "cursor over the panel captures"
        );

        // Outside the panel the pointer passes through to the desktop.
        (app.state.cursor_x, app.state.cursor_y) = (1.0, 1.0);
        assert!(!app.panel_captures_cursor());
        assert!(!app.cursor_entry_signature().capture);
        assert!(!app.panel_left_button(true, true));
        assert!(!app.panel_other_button(Right, true, true));
        assert!(!app.panel_other_button(Right, false, true));

        let esc = (
            PhysicalKey::Code(KeyCode::Escape),
            Key::Named(NamedKey::Escape),
        );
        // Esc with a query clears it; a second Esc closes and resumes.
        assert!(app.panel_key_input(esc.0, &esc.1, true));
        wait("4", || {
            panel.current_model().is_some_and(|m| m.query.is_empty())
        })
        .await;
        assert!(app.panel_key_input(esc.0, &esc.1, true));
        wait("5", || !panel.is_open()).await;
        assert!(
            !app.state.safe_mode_atomic.load(Ordering::Acquire),
            "closing resumes"
        );
        assert!(
            !app.panel_key_input(esc.0, &esc.1, false),
            "closed: release flows on"
        );
        assert!(!app.panel_captures_cursor());
        assert!(
            !app.cursor_entry_armed(),
            "closed panel, empty scene: no raw-motion wakes"
        );
    }

    /// An edit that waits on something slow (an administrator prompt, a
    /// download) must not hold the human override: the row shows it is
    /// working, a second press does not start another, Esc still closes and
    /// resumes agents, and the outcome shows when the panel is reopened.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_edit_runs_beside_the_panel_and_esc_still_resumes() {
        use crate::shell::control_panel::Section;
        use crate::shell::control_panel::row::{BoxFuture, Control, Edit, Row};

        struct Slow {
            started: Arc<AtomicUsize>,
            release: Arc<tokio::sync::Notify>,
        }
        impl Section for Slow {
            fn id(&self) -> &'static str {
                "slow"
            }
            fn title(&self) -> &'static str {
                "Slow"
            }
            fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
                Box::pin(async {
                    vec![
                        Row::new("slow.go", "Go", Control::Action { confirm: None }).detail("Idle"),
                    ]
                })
            }
            fn apply<'a>(&'a self, _: &'a str, _: Edit) -> BoxFuture<'a, Result<(), String>> {
                Box::pin(async move {
                    self.started.fetch_add(1, Ordering::SeqCst);
                    self.release.notified().await;
                    Err("The prompt was declined".to_owned())
                })
            }
        }

        let state = WindowedRuntimeState::new_headless();
        let deps = ControlPanelDeps {
            scene: Arc::clone(&state.shared_state.lock().await.scene),
            pause: PauseControl::new(
                Arc::clone(&state.shared_state),
                Arc::clone(&state.chrome_state),
                Arc::clone(&state.safe_mode_atomic),
                RenderWakeNotifier::default(),
                Some(state.portal_projection_driver.portal_handle()),
                state.wake.main_work_notifier(),
            ),
            chords: crate::shell::control_panel::chords::ChordControl::fixed(HudChords::default()),
            pairing: None,
            agents: None,
            connect: None,
            runtime: None,
            appearance: None,
            render_wake: RenderWakeNotifier::default(),
            main_wake: RenderWakeNotifier::default(),
        };
        let (started, release) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(tokio::sync::Notify::new()),
        );
        let mut sections = default_sections(&deps);
        sections.push(Box::new(Slow {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        }));
        let panel = spawn_control_panel(
            &tokio::runtime::Handle::current(),
            deps,
            sections,
            state.shutdown.clone(),
        );
        let paused = || state.safe_mode_atomic.load(Ordering::Acquire);
        let detail = || {
            let Some(PanelBody::Sections(cards)) = panel.current_model().map(|m| m.body) else {
                return None;
            };
            cards
                .iter()
                .flat_map(|c| &c.items)
                .find(|i| i.id == "slow.go")
                .and_then(|i| i.detail.clone())
        };
        let go = PanelHitTarget::Row("slow.go".into(), PanelPart::Body);

        panel.send(PanelEvent::Chord(ChordRole::Pause));
        wait("paused and open", || paused() && panel.is_open()).await;
        panel.send(PanelEvent::Click(go.clone()));
        wait("working", || {
            detail().is_some_and(|d| d.starts_with("Working"))
        })
        .await;
        panel.send(PanelEvent::Click(go));
        panel.send(PanelEvent::Key(PanelKey::Escape));
        wait("closed and resumed while the edit runs", || {
            !panel.is_open() && !paused()
        })
        .await;
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "a busy row starts nothing new"
        );

        panel.send(PanelEvent::Chord(ChordRole::Panel));
        wait("reopened, still working", || {
            detail().is_some_and(|d| d.starts_with("Working"))
        })
        .await;
        release.notify_one();
        wait("the outcome shows", || {
            detail().as_deref() == Some("The prompt was declined")
        })
        .await;

        // A failure while the panel is closed shows when it reopens.
        panel.send(PanelEvent::Click(PanelHitTarget::Row(
            "slow.go".into(),
            PanelPart::Body,
        )));
        wait("working again", || {
            detail().is_some_and(|d| d.starts_with("Working"))
        })
        .await;
        panel.send(PanelEvent::Key(PanelKey::Escape));
        wait("closed", || !panel.is_open()).await;
        release.notify_one();
        // Events are handled in order: once the outcome is queued, the
        // reopen chord lands after it, on a closed panel.
        wait("the outcome is queued", || panel.applied_sent() == 2).await;
        panel.send(PanelEvent::Chord(ChordRole::Panel));
        wait("the closed-time failure shows", || {
            detail().as_deref() == Some("The prompt was declined")
        })
        .await;
        panel.send(PanelEvent::Key(PanelKey::Escape));
        wait("closed", || !panel.is_open()).await;
    }

    async fn wait(what: &str, done: impl Fn() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("panel never reached state {what}");
    }

    /// A section that panics must not take the human override with it: a
    /// panicking `rows` shows one error row and a panicking `apply` fails its
    /// row, while the panel stays usable and the pause holds.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_panicking_section_fails_alone_and_the_pause_holds() {
        use crate::shell::control_panel::Section;
        use crate::shell::control_panel::row::{BoxFuture, Control, Edit, Row};

        struct Broken {
            rows_panic: bool,
        }
        impl Section for Broken {
            fn id(&self) -> &'static str {
                if self.rows_panic { "norows" } else { "noapply" }
            }
            fn title(&self) -> &'static str {
                "Broken"
            }
            fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
                Box::pin(async move {
                    assert!(!self.rows_panic, "section bug in rows");
                    vec![Row::new("noapply.boom", "Boom", Control::Toggle(false))]
                })
            }
            fn apply<'a>(&'a self, _: &'a str, _: Edit) -> BoxFuture<'a, Result<(), String>> {
                Box::pin(async { panic!("section bug in apply") })
            }
        }

        let state = WindowedRuntimeState::new_headless();
        let deps = ControlPanelDeps {
            scene: Arc::clone(&state.shared_state.lock().await.scene),
            pause: PauseControl::new(
                Arc::clone(&state.shared_state),
                Arc::clone(&state.chrome_state),
                Arc::clone(&state.safe_mode_atomic),
                RenderWakeNotifier::default(),
                Some(state.portal_projection_driver.portal_handle()),
                state.wake.main_work_notifier(),
            ),
            chords: crate::shell::control_panel::chords::ChordControl::fixed(HudChords::default()),
            pairing: None,
            agents: None,
            connect: None,
            runtime: None,
            appearance: None,
            render_wake: RenderWakeNotifier::default(),
            main_wake: RenderWakeNotifier::default(),
        };
        let mut sections = default_sections(&deps);
        sections.push(Box::new(Broken { rows_panic: true }));
        sections.push(Box::new(Broken { rows_panic: false }));
        let panel = spawn_control_panel(
            &tokio::runtime::Handle::current(),
            deps,
            sections,
            state.shutdown.clone(),
        );
        let paused = || state.safe_mode_atomic.load(Ordering::Acquire);
        let detail = |id: &str| {
            let Some(PanelBody::Sections(cards)) = panel.current_model().map(|m| m.body) else {
                return None;
            };
            cards
                .iter()
                .flat_map(|c| &c.items)
                .find(|i| i.id == id)
                .map(|i| (i.label.clone(), i.detail.clone()))
        };

        panel.send(PanelEvent::Chord(ChordRole::Pause));
        wait("paused and open", || paused() && panel.is_open()).await;
        assert_eq!(
            detail("norows.unavailable")
                .map(|(label, _)| label)
                .as_deref(),
            Some("This section failed to load")
        );

        let boom = PanelHitTarget::Row("noapply.boom".into(), PanelPart::Body);
        panel.send(PanelEvent::Click(boom));
        wait("apply failure shown", || {
            detail("noapply.boom").and_then(|(_, d)| d).as_deref()
                == Some("This setting failed; see the HUD log")
        })
        .await;
        assert!(panel.is_open() && paused(), "the panel and the pause stand");

        panel.send(PanelEvent::Key(PanelKey::Escape));
        wait("closed and resumed", || !panel.is_open() && !paused()).await;
    }
}
