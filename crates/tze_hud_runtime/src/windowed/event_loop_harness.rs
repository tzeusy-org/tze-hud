//! # event_loop_harness
//!
//! Headless test harness that drives the runtime's real event/window
//! state-machine — the code production runs inside winit's
//! [`ApplicationHandler`] — with synthetic events, WITHOUT constructing a real
//! `winit` window, a `wgpu` surface, or running the OS event loop.
//!
//! ## Why this exists
//!
//! Several keyboard-drain tests historically had to *reconstruct* the
//! production drain loop (a hand-written `for _ in 0..limit` over a local
//! `VecDeque`, calling `InputProcessor::route_character_to_composer` directly)
//! because there was no way to construct a [`WinitApp`] without a live window /
//! GPU. That reconstruction can silently drift from the real
//! [`WinitApp::drain_pending_keyboard_events`] path (active-tab resolution →
//! inner-fn dispatch → [`restore_front_requeued_event`]).
//!
//! This harness closes that gap. The runtime's event/window state machine is
//! already decoupled from winit at the method boundary: the drain and the
//! `dispatch_*_event_inner` fns are methods on [`WinitApp`] that take the
//! runtime's *own* `PendingKeyboardEvent` type (never winit event types) and
//! never touch `ActiveEventLoop`. The only thing that previously blocked a test
//! from driving them was the inability to build a [`WindowedRuntimeState`]. The
//! [`WindowedRuntimeState::new_headless`] constructor below supplies an inert
//! but real state (no window, no GPU, no network servers), and
//! [`HeadlessEventLoopHarness`] wraps a real [`WinitApp`] around it so tests can
//! inject synthetic keyboard events and run the genuine production dispatch.
//!
//! ## Scope
//!
//! Construction stays GPU-free. An opt-in test/dev offscreen backend uses the
//! canonical HeadlessRuntime factory and the same scene/settle/production turn.
//! It measures real local GPU submissions, never OS presentation or scanout.

use std::net::SocketAddr;
use std::sync::Arc;

#[cfg(test)]
use tze_hud_input::PointerEvent;
use tze_hud_input::{
    FocusManager, InputProcessor, KeyboardModifiers, PointerEventKind, RawCharacterEvent,
    RawKeyDownEvent, RawKeyUpEvent,
};
use tze_hud_scene::graph::SceneGraph;
#[cfg(test)]
use tze_hud_scene::types::ZoneInteractionKind;
use tze_hud_scene::{Clock, MonoUs, NodeData, SceneId};
#[cfg(test)]
use tze_hud_scene::{Node, Rect, types::HitRegionNode};

use tze_hud_protocol::proto::session::hud_session_server::HudSessionServer;
use tze_hud_protocol::session_server::{HudSessionImpl, SessionDeps};

use super::WindowedRuntimeState;
use super::WinitApp;
#[cfg(test)]
use super::keyboard::PendingKeyboardEvent;

impl WindowedRuntimeState {
    /// Build an inert `WindowedRuntimeState` for headless state-machine tests.
    ///
    /// Mirrors the field construction in [`super::WindowedRuntime::run`] but
    /// omits everything that needs a display, a GPU, or a live network runtime:
    ///
    /// - `window` / `window_surface` / `compositor` / `compositor_handle` — `None`.
    /// - `network_rt` / `network_handles` — no gRPC/MCP servers are spawned.
    /// - `degradation_notices` — `None` until networking is explicitly enabled.
    /// - portal state is an empty synchronous Hub; no network ingress is spawned.
    ///
    /// The `safe_mode_atomic` and `active_tab_mirror` `Arc`s are shared between
    /// the state and its embedded [`SharedState`] exactly as production does, so
    /// the lock-free keyboard-dispatch reads observe the same values a real run
    /// would.
    pub(super) fn new_headless() -> Self {
        Self::new_headless_with_clock(Arc::new(tze_hud_scene::SystemClock::new()))
    }

    pub(super) fn new_headless_with_clock(clock: Arc<dyn Clock>) -> Self {
        use std::collections::{HashMap, HashSet, VecDeque};
        use std::sync::Mutex as StdMutex;
        use std::sync::atomic::AtomicBool;

        use tokio::sync::Mutex as TokioMutex;

        let safe_mode_atomic = Arc::new(AtomicBool::new(false));
        let active_tab_mirror: Arc<StdMutex<Option<SceneId>>> = Arc::new(StdMutex::new(None));

        let portals = tze_hud_mcp::PortalHandle::new(
            tze_hud_portal::hub::PortalHub::default(),
            Arc::clone(&clock),
        );
        let shared_state = Arc::new(TokioMutex::new(SharedStateBuilder::build(
            Arc::clone(&safe_mode_atomic),
            Arc::clone(&active_tab_mirror),
            clock,
        )));

        let (frame_ready_tx, frame_ready_rx) = crate::channels::frame_ready_channel();

        WindowedRuntimeState {
            wake: super::wake::WindowedWake::disconnected(),
            scheduled_main_deadline: None,
            cursor_entry: super::wake::CursorEntryState::default(),
            config: super::WindowedConfig::default(),
            compositor_handle: None,
            network_rt: None,
            network_handles: Vec::new(),
            runtime_context: Arc::new(crate::runtime_context::RuntimeContext::headless_default()),
            _runtime_widget_store: None,
            shared_state,
            safe_mode_atomic,
            active_tab_mirror,
            chrome_state: Arc::new(std::sync::RwLock::new(crate::shell::ChromeState::new())),
            system_card: crate::shell::system_card::SystemCardHandle::default(),
            live: Arc::new(crate::launch::LiveSettings::new(
                60,
                false,
                Default::default(),
            )),
            control_panel: None,
            pending_input_latency: Arc::new(StdMutex::new(VecDeque::new())),
            frame_ready_rx,
            deferred_present: false,
            frame_ready_tx: Some(frame_ready_tx),

            degradation_notices: None,
            window_surface: None,
            input_processor: InputProcessor::new(),
            focus_manager: FocusManager::new(),

            pipeline: crate::pipeline::FramePipeline::new(),
            shutdown: crate::threads::ShutdownToken::new(),
            quiescent_failed: Arc::new(AtomicBool::new(false)),
            terminal_surface_recovery_failed: Arc::new(AtomicBool::new(false)),
            quiescent_efficiency: None,
            cursor_x: 0.0,
            cursor_y: 0.0,
            left_button_down: false,
            cursor_tracker: tze_hud_input::CursorIconTracker::new(),
            window: None,
            effective_mode: crate::window::WindowMode::Fullscreen,
            hit_regions: Vec::new(),
            overlay_capturing: false,
            static_hit_regions: Vec::new(),
            widget_hover_trackers: HashMap::new(),
            pending_widget_svgs: Vec::new(),
            startup_diagnostics: None,
            text_fit: Default::default(),
            modifiers: winit::keyboard::ModifiersState::empty(),
            primary_monitor: None,
            secondaries: Vec::new(),
            display_targets: Default::default(),
            placement_layout: Default::default(),
            displays_dirty: Default::default(),
            surface_factory: None,
            zone_displays: Default::default(),
            display_keys: Vec::new(),
            zone_targets: Default::default(),
            cursor_window: None,
            keyboard_window: None,
            secondary_recreates: Default::default(),
            overlay_refits: Default::default(),
            press_window: None,
            zone_press: None,
            press_guard: Default::default(),
            notification_link_opener: Default::default(),
            link_feedback: None,
            notification_clipboard: Default::default(),
            copy_feedback: None,
            notification_reveal: Default::default(),
            window_go_to: Default::default(),
            context_menu_press: None,
            context_menu_open: false,
            context_menu_dismiss_pending: false,
            notification_keydowns: Default::default(),
            answer_keyboard: Default::default(),
            primary_drag_gesture: None,
            global_tokens: HashMap::new(),
            resize_affordance_px: Default::default(),
            cursor_shape_window: None,
            appearance: None,
            tokens_seen: 0,
            capture_seen: 0,
            _config_watch: None,

            composer_pointer_drag_anchor: None,
            reading_cursor: None,
            pressable_regions: Vec::new(),
            portal_resize_states: HashMap::new(),
            consumed_portal_resize_keydowns: HashSet::new(),
            consumed_shell_shortcut_keydowns: HashSet::new(),
            panel_input: crate::windowed::panel_input::PanelInputState::default(),
            keyboard_activation_nodes: HashMap::new(),
            consumed_command_keydowns: HashSet::new(),
            local_composer_state: Arc::new(StdMutex::new(None)),
            composer_preedit: None,
            kept_composer_drafts_state: Arc::new(StdMutex::new(None)),
            kept_drafts_drawn: Vec::new(),
            focus_ring_owner_state: Arc::new(StdMutex::new(None)),
            surface_ui: Default::default(),
            resize_grip_hover_state: Arc::new(StdMutex::new(None)),
            tile_close_hover_state: Arc::new(StdMutex::new(None)),
            drag_handle_hover_state: Arc::new(StdMutex::new(None)),
            cursor_left_window: false,
            composer_visual_layout: Arc::new(StdMutex::new(None)),
            portal_projection_driver: crate::portal::PortalDriver::new(portals),
            capture_inbox: crate::operator::screenshot::CaptureInbox::detached(),
            pending_keyboard_events: VecDeque::new(),
            pending_scroll: VecDeque::new(),
            pending_pointer_move: false,
            pending_pointer_buttons: VecDeque::new(),
            interaction_feedback_lock_misses: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

/// Local builder for a minimal [`SharedState`], factored out only to keep the
/// long field list in [`WindowedRuntimeState::new_headless`] readable. Mirrors
/// [`super::test_support::make_shared_state`] but threads through the caller's
/// shared `safe_mode_atomic` / `active_tab_mirror` `Arc`s.
struct SharedStateBuilder;

impl SharedStateBuilder {
    fn build(
        safe_mode_atomic: Arc<std::sync::atomic::AtomicBool>,
        active_tab_mirror: Arc<std::sync::Mutex<Option<SceneId>>>,
        clock: Arc<dyn Clock>,
    ) -> tze_hud_protocol::session::SharedState {
        use tokio::sync::Mutex as TokioMutex;
        tze_hud_protocol::session::SharedState {
            scene: Arc::new(TokioMutex::new(SceneGraph::new_with_clock(
                1920.0, 1080.0, clock,
            ))),
            sessions: tze_hud_protocol::session::SessionRegistry::new(),
            resource_store: tze_hud_resource::ResourceStore::new(
                tze_hud_resource::ResourceStoreConfig::default(),
            ),
            runtime_widget_store: None,
            element_store: tze_hud_scene::element_store::ElementStore::default(),
            element_store_file: None,
            safe_mode_atomic,
            active_tab_mirror,
        }
    }
}

/// Drives the real [`WinitApp`] event/window state machine headlessly.
///
/// Wraps a [`WinitApp`] built from [`WindowedRuntimeState::new_headless`] and
/// exposes the small surface a keyboard-drain test needs: install a focused
/// composer, inject synthetic `PendingKeyboardEvent`s, run the genuine
/// [`WinitApp::drain_pending_keyboard_events`], and observe the resulting
/// composer draft. The entire body of the drain — active-tab resolution via the
/// lock-free mirror, per-event inner-fn dispatch, and
/// `restore_front_requeued_event` — runs through production code.
///
/// [`Self::with_network`] adds the MCP server and an injected clock for
/// end-to-end tests (see the module docs).
pub struct HeadlessEventLoopHarness {
    app: WinitApp,
    frame_turn: crate::pipeline::FrameTurnState,
    offscreen: Option<(
        tze_hud_compositor::Compositor,
        tze_hud_compositor::HeadlessSurface,
    )>,
    last_turn_submitted: bool,
    offscreen_work: (u64, u64),
    offscreen_tokens_seen: u64,
    mcp_addr: Option<SocketAddr>,
    grpc_addr: Option<SocketAddr>,
    clipboard: Arc<std::sync::Mutex<Vec<String>>>,
    /// What the panel's System and Network rows did outside the HUD
    /// (`open <path>`, `firewall <action> <ports>`, `autostart <on>`),
    /// recorded instead of done.
    effects: Arc<std::sync::Mutex<Vec<String>>>,
    /// The first-run pairing the panel's connect rows share, when booted
    /// with an agents file.
    pairing: Option<Arc<crate::pairing::Pairing>>,
    /// Already accepted overlay geometry, substituting for OS windows only.
    display_fixture: Option<(
        super::displays::MonitorSpec,
        Vec<super::displays::MonitorSpec>,
    )>,
}

impl HeadlessEventLoopHarness {
    /// Boot the GPU-free runtime from `cfg` with its MCP server listening on
    /// `127.0.0.1:0`, every deadline reading `clock` (invariant 9).
    ///
    /// Mirrors [`super::WindowedRuntime::run`]'s wiring: runtime context and
    /// agent directory from `cfg.agents`, scene startup (zones, widgets,
    /// design tokens), the MCP → event-loop portal-op channel, and the portal
    /// projection driver. It skips what needs a display or touches the user's
    /// disk (window, compositor, gRPC, element and widget-asset stores).
    /// `cfg.mcp_port` and `cfg.grpc_port` are ignored.
    pub async fn with_network(
        cfg: super::WindowedConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::with_network_and_agents_file(cfg, clock, None).await
    }

    /// [`Self::with_network`] whose control panel changes agents in the
    /// `agents.toml` at `agents_path` (which should describe `cfg.agents`).
    /// Without one, Unpair and allow-list edits fail.
    pub async fn with_network_and_agents_file(
        cfg: super::WindowedConfig,
        clock: Arc<dyn Clock>,
        agents_path: Option<std::path::PathBuf>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let runtime_context = super::build_runtime_context(&cfg);
        let mut state = WindowedRuntimeState::new_headless();

        let mut scene = SceneGraph::new_with_clock(
            cfg.window.width as f32,
            cfg.window.height as f32,
            Arc::clone(&clock),
        );
        let raw: Option<tze_hud_config::raw::RawConfig> =
            cfg.config_toml.as_deref().map(toml::from_str).transpose()?;
        state.zone_displays = raw
            .as_ref()
            .map(tze_hud_config::displays::zone_display_assignments)
            .unwrap_or_default();
        let global_tokens = match &raw {
            Some(raw) => {
                let config_parent = cfg
                    .config_file_path
                    .as_deref()
                    .and_then(|p| std::path::Path::new(p).parent());
                let startup = crate::scene_startup::run_scene_startup(
                    raw,
                    config_parent,
                    &mut scene,
                    tze_hud_config::tokens::OsPreferences::default(),
                );
                for (widget_type, svg_file, svg) in &startup.widget_svg_assets {
                    state
                        .text_fit
                        .register_widget_svg(widget_type, svg_file, svg);
                }
                state.pending_widget_svgs.extend(startup.widget_svg_assets);
                startup.global_tokens
            }
            None => {
                scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
                std::collections::HashMap::new()
            }
        };
        let scene_handle = {
            let shared = state.shared_state.lock().await;
            *shared.scene.lock().await = scene;
            Arc::clone(&shared.scene)
        };

        let portals = tze_hud_mcp::PortalHandle::new(
            tze_hud_portal::hub::PortalHub::default(),
            Arc::clone(&clock),
        );
        let grpc_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let grpc_addr = grpc_listener.local_addr()?;
        // Pairing, as production builds it, when there is a store to pair into.
        let has_store = agents_path.is_some();
        let store = Arc::new(crate::agent_store::AgentStore::open(
            agents_path,
            Arc::clone(&cfg.agents),
        ));
        let pairing = has_store.then(|| {
            Arc::new(crate::pairing::Pairing::system(
                Arc::clone(&store),
                state.system_card.clone(),
                grpc_addr.port(),
            ))
        });
        let render_wake = state.wake.render_notifier();
        state.live = Arc::new(
            crate::launch::LiveSettings::new(cfg.target_fps, cfg.debug_zones, render_wake.clone())
                .with_answer_window(cfg.answer_window_ms),
        );
        // GPU-free: measure with the production tokens the compositor would use.
        state.text_fit.set_tokens(global_tokens.clone());
        let mcp_config = crate::mcp::McpServerConfig {
            startup: None,
            text_fit: Some(Arc::clone(&state.text_fit) as Arc<dyn tze_hud_scene::TextFit>),
            appearance: None,
            publication_defaults: state.live.publication_defaults.clone(),
            widget_transition_ms: tze_hud_config::tokens::resolve_motion_duration_ms(
                &global_tokens,
                "motion.state.ms",
            ),
            bind_addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
            late_tailnet_port: None,
            agents: Arc::clone(&cfg.agents),
            presents: Some(Arc::clone(state.wake.counters())),
            surfaces: Some(crate::operator::surface_status::probe(
                Arc::clone(&scene_handle),
                Arc::clone(&state.surface_ui),
                Arc::clone(&state.notification_link_opener.launches),
            )),
            capture: None,
            restart: None,
            update: None,
            bind_gate: None,
            pairing: pairing.clone(),
        };
        let (_mcp_task, mcp_addrs) = crate::mcp::start_mcp_http_server_with_render_wake(
            scene_handle,
            mcp_config,
            state.shutdown.clone(),
            Some(portals.clone()),
            render_wake.clone(),
            state.wake.main_work_notifier(),
            Arc::clone(&state.safe_mode_atomic),
        )
        .await?;
        let mcp_addr = mcp_addrs[0];
        if let Some(pairing) = &pairing {
            // Bound on port 0: hand out the port the listener got.
            *pairing
                .binds(&mcp_addrs)
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = mcp_addrs.clone();
        }

        // The gRPC session server over the same `SharedState` (so one scene,
        // one session registry, one safe-mode flag), built as production
        // builds it. Its lease-expiry channel is the one `tick` publishes
        // sweep results on.
        let service = HudSessionImpl::from_deps(SessionDeps {
            publication_defaults: state.live.publication_defaults.clone(),
            resource_budget: runtime_context.resource_budget(),
            budget_enforcer: Some(Arc::new(
                crate::mutation_budget_bridge::RuntimeMutationBudgetEnforcer::with_limits(
                    runtime_context.operational_envelope.max_resident_sessions,
                ),
            )),
            ..SessionDeps::new(Arc::clone(&state.shared_state), Arc::clone(&cfg.agents))
        });
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(HudSessionServer::new(service))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                    grpc_listener,
                )),
        );

        let panel_scene = Arc::clone(&state.shared_state.lock().await.scene);
        let appearance = Arc::new(crate::appearance::Appearance::for_startup(
            raw.as_ref(),
            cfg.config_file_path.as_deref().map(std::path::Path::new),
            global_tokens.clone(),
            Arc::clone(&panel_scene),
            render_wake.clone(),
            state.wake.main_work_notifier(),
            // Harnesses never depend on the host's Windows preferences.
            tze_hud_config::tokens::OsPreferences::default,
        ));
        state.appearance = Some(Arc::clone(&appearance));
        let clipboard: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let effects: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let deps = crate::shell::control_panel::ControlPanelDeps {
            clipboard: Default::default(),
            display_probe: Arc::new(crate::question_placement::Win32Probe),
            display_layout: Arc::clone(&state.placement_layout),
            scene: Arc::clone(&panel_scene),
            pause: crate::shell::control_panel::PauseControl::new(
                Arc::clone(&state.shared_state),
                Arc::clone(&state.chrome_state),
                Arc::clone(&state.safe_mode_atomic),
                render_wake.clone(),
                Some(portals.clone()),
                state.wake.main_work_notifier(),
            ),
            // Windows' registrar, as a record: every rebind takes.
            chords: {
                let effects = Arc::clone(&effects);
                crate::shell::control_panel::chords::ChordControl::new(
                    runtime_context.chords,
                    Some(Arc::new(move |role, hotkey| {
                        effects
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(format!("rebind {role:?} {hotkey}"));
                        Ok(())
                    })),
                )
            },
            pairing: pairing.clone(),
            connect: pairing.clone().map(|pairing| {
                let clipboard = Arc::clone(&clipboard);
                crate::shell::control_panel::ConnectControl {
                    pairing,
                    store: Arc::clone(&store),
                    clipboard: Arc::new(move |text: &str| {
                        clipboard
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(text.to_owned());
                        Ok(())
                    }),
                    machine: Some("test-pc".to_owned()),
                }
            }),
            agents: Some(crate::shell::control_panel::AgentControl {
                store,
                shared: Arc::clone(&state.shared_state),
                scene: panel_scene,
                portals: Some(portals.clone()),
                pairing: pairing.clone(),
                render_wake: render_wake.clone(),
                main_wake: state.wake.main_work_notifier(),
            }),
            runtime: Some(Self::panel_runtime(
                &cfg,
                Arc::clone(&state.live),
                pairing.as_ref().map(|p| p.firewall()),
                Arc::clone(&effects),
            )),
            appearance: Some(appearance),
            render_wake,
            main_wake: state.wake.main_work_notifier(),
        };
        let sections = crate::shell::control_panel::default_sections(&deps);
        state.control_panel = Some(crate::shell::control_panel::spawn_control_panel(
            &tokio::runtime::Handle::current(),
            deps,
            sections,
            state.shutdown.clone(),
        ));

        if let Some(panel) = &state.control_panel {
            panel.set_answer(
                state
                    .answer_keyboard
                    .tray_hook(state.wake.main_work_notifier()),
            );
        }
        let driver = crate::portal::PortalDriver::new(portals);
        state.portal_projection_driver = driver;
        state.global_tokens = global_tokens;
        state.resize_affordance_px.set(None);
        state.runtime_context = runtime_context;
        state.config = cfg;
        Ok(HeadlessEventLoopHarness {
            app: WinitApp { state },
            frame_turn: crate::pipeline::FrameTurnState::default(),
            offscreen: None,
            last_turn_submitted: false,
            offscreen_work: (0, 0),
            offscreen_tokens_seen: 0,
            mcp_addr: Some(mcp_addr),
            grpc_addr: Some(grpc_addr),
            clipboard,
            effects,
            pairing,
            display_fixture: None,
        })
    }

    /// The shared scene, locked. Session and MCP tasks on the network runtime
    /// hold it briefly, so this waits them out (the test thread holds nothing
    /// else, so it cannot deadlock).
    fn scene(&self) -> tokio::sync::OwnedMutexGuard<SceneGraph> {
        let deadline = std::time::Instant::now() + SCENE_WAIT;
        loop {
            if let Ok(state) = self.app.state.shared_state.try_lock() {
                let scene = Arc::clone(&state.scene);
                drop(state);
                if let Ok(guard) = scene.try_lock_owned() {
                    return guard;
                }
            }
            assert!(std::time::Instant::now() < deadline, "{BUSY}");
            std::thread::yield_now();
        }
    }

    /// The MCP endpoint `with_network` bound.
    ///
    /// # Panics
    ///
    /// On a harness built without [`Self::with_network`].
    pub fn mcp_addr(&self) -> SocketAddr {
        self.mcp_addr
            .expect("harness was built without with_network")
    }

    /// Observe the actual armed wait of one correlated MCP input request.
    #[cfg(feature = "test-harness")]
    pub fn observe_input_wait(&self, namespace: &str) -> tze_hud_mcp::server::InputWaitProbe {
        self.app
            .state
            .portal_projection_driver
            .portal_handle()
            .observe_input_wait(namespace)
    }

    /// The gRPC session endpoint `with_network` bound (loopback, ephemeral).
    ///
    /// # Panics
    ///
    /// On a harness built without [`Self::with_network`].
    pub fn grpc_addr(&self) -> SocketAddr {
        self.grpc_addr
            .expect("harness was built without with_network")
    }

    /// Press the human pause chord: the event the Windows hotkey thread sends
    /// the control panel. The panel task pauses (or resumes) agents on the
    /// network runtime; [`Self::safe_mode_active`] shows when it has.
    pub fn press_safe_mode_hotkey(&self) {
        self.press_chord(tze_hud_scene::config::ChordRole::Pause);
    }

    /// Feed the same explicit menu intent as the native tray callback, without
    /// advancing the winit harness. The real panel task applies it asynchronously.
    pub fn tray_command(&self, action: &str) {
        use crate::shell::control_panel::{PanelEvent, TrayCommand};
        let command = match action {
            "toggle_panel" => TrayCommand::TogglePanel,
            "answer_next" => TrayCommand::AnswerNext,
            "open_panel" => TrayCommand::OpenPanel,
            "pause" => TrayCommand::Pause,
            "resume" => TrayCommand::Resume,
            "hide_all" => TrayCommand::HideAll,
            "show_all" => TrayCommand::ShowAll,
            "quit" => TrayCommand::Quit,
            _ => panic!("unknown tray test command"),
        };
        assert!(
            self.control_panel()
                .send(PanelEvent::Tray(command, Default::default()))
        );
    }

    /// Press one of the HUD's global chords.
    pub fn press_chord(&self, role: tze_hud_scene::config::ChordRole) {
        let panel = self
            .app
            .state
            .control_panel
            .as_ref()
            .expect("harness was built without with_network");
        assert!(
            panel.send(crate::shell::control_panel::PanelEvent::Chord(role)),
            "control panel task is running"
        );
    }

    fn control_panel(&self) -> &crate::shell::control_panel::ControlPanelHandle {
        self.app
            .state
            .control_panel
            .as_ref()
            .expect("harness was built without with_network")
    }

    /// The open control panel's row `row` as last published (in a section
    /// card, or a search result), or `None`.
    pub fn control_panel_row(&self, row: &str) -> Option<tze_hud_compositor::PanelItem> {
        let model = self.control_panel().current_model()?;
        match model.body {
            tze_hud_compositor::PanelBody::Sections(sections) => sections
                .into_iter()
                .flat_map(|s| s.items)
                .find(|item| item.id == row),
            tze_hud_compositor::PanelBody::Results { items, .. } => {
                items.into_iter().find(|item| item.id == row)
            }
            tze_hud_compositor::PanelBody::Connect(card) => {
                card.items.into_iter().find(|item| item.id == row)
            }
            tze_hud_compositor::PanelBody::Empty(_) => None,
        }
    }

    /// The current topology's zone choice as actually published by the panel.
    pub fn control_panel_zone_row(&self, zone: &str) -> Option<tze_hud_compositor::PanelItem> {
        let displays = self.app.state.control_panel.as_ref()?.displays.as_ref()?;
        let topology = displays.snapshot().2;
        self.control_panel_row(&format!("layout.zone.{zone}.{topology}"))
    }

    /// Inject accepted monitor keys/overlay geometry, bypassing OS identity
    /// discovery and window creation. The real topology adoption and publisher
    /// feed the panel, status and compositor's shared DisplayLayout. Subsequent
    /// ticks run the same pending-assignment drain as `about_to_wait`.
    pub fn set_display_topology(
        &mut self,
        monitors: Vec<(tze_hud_compositor::DisplayRect, String)>,
    ) {
        use super::displays::{KeyedMonitor, MonitorSpec};
        use super::monitor_identity::DisplayKey;
        assert_eq!(monitors.iter().filter(|(m, _)| m.primary).count(), 1);
        let mut keyed = Vec::new();
        let mut primary = None;
        let mut secondaries = Vec::new();
        for (monitor, key) in monitors {
            let spec = MonitorSpec {
                name: monitor.name.clone(),
                x: monitor.rect.x as i32,
                y: monitor.rect.y as i32,
                width: monitor.rect.width as u32,
                height: monitor.rect.height as u32,
                scale: 1.0,
            };
            keyed.push(KeyedMonitor {
                name: monitor.name,
                key: DisplayKey {
                    base: key.split_once('@').map(|(base, _)| base.to_owned()),
                    key,
                    stable: true,
                },
            });
            if monitor.primary {
                primary = Some(spec);
            } else {
                secondaries.push(spec);
            }
        }
        let primary = primary.expect("one primary");
        self.app.adopt_display_keys(keyed, &[]);
        self.app.state.primary_monitor = Some(primary.clone());
        self.app.publish_displays_for(&primary, &secondaries);
        self.display_fixture = Some((primary, secondaries));
    }

    /// Restrict the fixture's secondary overlay plan without removing any
    /// connected hardware keys. This models primary-only modes or a failed
    /// sibling overlay; `set_display_topology` restores the complete plan.
    pub fn retain_display_overlays(&mut self, secondary_names: &[&str]) {
        let (primary, secondaries) = self
            .display_fixture
            .as_mut()
            .expect("display topology fixture");
        secondaries.retain(|monitor| secondary_names.contains(&monitor.name.as_str()));
        self.app.publish_displays_for(primary, secondaries);
    }

    /// Observe the compositor's actual shared placement (None means primary),
    /// and its publication generation, without constructing GPU surfaces.
    pub fn zone_display_placement(&self, zone: &str) -> (Option<tze_hud_scene::Rect>, u64) {
        let targets = self
            .app
            .state
            .display_targets
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (targets.layout.zone_display_rect(zone), targets.generation)
    }

    /// The open control panel's Connect an agent card, if it is showing.
    pub fn control_panel_connect(&self) -> Option<tze_hud_compositor::PanelConnect> {
        match self.control_panel().current_model()?.body {
            tze_hud_compositor::PanelBody::Connect(card) => Some(card),
            _ => None,
        }
    }

    /// The panel's Network, Diagnostics and System deps as production builds
    /// them from `cfg` (config file, sources, ports), with every outside
    /// effect recorded in `effects` and a restart whose new instance never
    /// starts (so the failure path shows).
    fn panel_runtime(
        cfg: &super::WindowedConfig,
        live: Arc<crate::launch::LiveSettings>,
        firewall: Option<Arc<crate::firewall::FirewallProbe>>,
        effects: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> crate::shell::control_panel::RuntimeControl {
        let restart = crate::operator::handoff::RestartHandle::with_spawner(
            Box::new(|_| Err(std::io::Error::other("test harness: no new instance"))),
            Arc::new(|| {}),
            std::time::Duration::from_millis(200),
            Default::default(),
        );
        let mut control = crate::shell::control_panel::RuntimeControl::system(
            cfg,
            live,
            Some(restart),
            None,
            firewall,
        );
        let record = move |effects: &Arc<std::sync::Mutex<Vec<String>>>, what: String| {
            effects.lock().unwrap_or_else(|e| e.into_inner()).push(what);
        };
        let (e1, e2, e3) = (
            Arc::clone(&effects),
            Arc::clone(&effects),
            Arc::clone(&effects),
        );
        control.remote = Arc::new(move |action, ports| {
            record(&e1, format!("firewall {action:?} {ports:?}"));
            Ok("done".to_owned())
        });
        control.open = Arc::new(move |path| {
            record(&e2, format!("open {}", path.display()));
            Ok(())
        });
        let autostart = Arc::new(std::sync::atomic::AtomicBool::new(false));
        control.autostart = Some(Arc::new(move |set| {
            if let Some(on) = set {
                autostart.store(on, std::sync::atomic::Ordering::SeqCst);
                record(&e3, format!("autostart {on}"));
            }
            Ok(autostart.load(std::sync::atomic::Ordering::SeqCst))
        }));
        control
    }

    /// What the panel's rows did outside the HUD, oldest first.
    pub fn panel_effects(&self) -> Vec<String> {
        self.effects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// A design token as the main thread draws with it now (after `tick`).
    pub fn design_token(&self, key: &str) -> Option<String> {
        self.app.state.global_tokens.get(key).cloned()
    }

    /// The frame rate cap and zone outlines the compositor would use now.
    pub fn live_settings(&self) -> (u32, bool) {
        (
            self.app.state.live.fps(),
            self.app.state.live.zone_outlines(),
        )
    }

    /// What the control panel last put on the clipboard (the harness
    /// clipboard records instead of touching the OS one).
    pub fn clipboard(&self) -> Vec<String> {
        self.clipboard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The open control panel's header state line.
    pub fn control_panel_state_line(&self) -> Option<String> {
        self.control_panel().current_model().map(|m| m.state_line)
    }

    /// Where keyboard focus is in the open control panel.
    pub fn control_panel_focus(&self) -> Option<tze_hud_compositor::PanelFocus> {
        self.control_panel().current_model().map(|m| m.focus)
    }

    /// Click the body of row `row` in the open control panel (press and
    /// release on it), as the winit loop reports a click.
    pub fn control_panel_click(&self, row: &str) {
        assert!(
            self.control_panel()
                .send(crate::shell::control_panel::PanelEvent::Click(
                    tze_hud_compositor::PanelHitTarget::Row(
                        row.to_owned(),
                        tze_hud_compositor::PanelPart::Body,
                    ),
                )),
            "control panel task is running"
        );
    }

    /// Open gRPC sessions for `agent`.
    pub async fn agent_sessions(&self, agent: &str) -> usize {
        self.app
            .state
            .shared_state
            .lock()
            .await
            .sessions
            .connected(agent)
    }

    /// Whether the control panel is open.
    /// Whether a pairing code is open (booted with an agents file).
    pub fn pairing_open(&self) -> bool {
        self.pairing
            .as_ref()
            .is_some_and(|pairing| pairing.is_open())
    }

    pub fn control_panel_open(&self) -> bool {
        self.app
            .state
            .control_panel
            .as_ref()
            .is_some_and(|panel| panel.is_open())
    }

    /// Whether safe mode is on (agents paused).
    pub fn safe_mode_active(&self) -> bool {
        self.app
            .state
            .safe_mode_atomic
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether the server's send buffer to `agent`'s session is full: the agent
    /// has stopped reading, so the next reply it is owed would block.
    pub async fn session_backed_up(&self, agent: &str) -> bool {
        let state = self.app.state.shared_state.lock().await;
        state
            .sessions
            .session_for_namespace(agent)
            .and_then(|session| session.server_message_tx.as_ref())
            .is_some_and(|tx| tx.capacity() == 0)
    }

    /// Number of leases safe mode has suspended.
    pub fn suspended_lease_count(&self) -> usize {
        let scene = self.scene();
        scene
            .leases
            .values()
            .filter(|lease| lease.state == tze_hud_scene::types::LeaseState::Suspended)
            .count()
    }

    /// Where each tile on screen sits, as the runtime resolved it.
    pub fn tile_bounds(&self) -> Vec<tze_hud_scene::Rect> {
        let scene = self.scene();
        scene.tiles.values().map(|tile| tile.bounds).collect()
    }

    /// Attach a real offscreen compositor using the canonical headless factory.
    /// The temporary factory scene is discarded; every turn uses this harness's
    /// actual shared scene, injected clock and production portal/composer state.
    /// Repeated calls retain the already attached backend and its frame ledger.
    pub async fn enable_offscreen_rendering(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.offscreen.is_some() {
            return Ok(());
        }
        let runtime = crate::headless::HeadlessRuntime::new(crate::headless::HeadlessConfig {
            width: self.app.state.config.window.width,
            height: self.app.state.config.window.height,
            grpc_port: 0,
            agents: self.app.state.config.agents.load().as_ref().clone(),
            config_toml: self.app.state.config.config_toml.clone(),
        })
        .await?;
        let mut compositor = runtime.compositor;
        crate::widget_runtime_registration::process_pending_widget_svgs(
            compositor.widget_renderer_mut(),
            self.app.state.pending_widget_svgs.drain(..),
        );
        compositor.set_token_map(self.app.state.global_tokens.clone());
        compositor.local_composer_state = Arc::clone(&self.app.state.local_composer_state);
        compositor.focus_ring_owner_state = Arc::clone(&self.app.state.focus_ring_owner_state);
        compositor.kept_composer_drafts_state =
            Arc::clone(&self.app.state.kept_composer_drafts_state);
        compositor.resize_grip_hover_state = Arc::clone(&self.app.state.resize_grip_hover_state);
        compositor.tile_close_hover_state = Arc::clone(&self.app.state.tile_close_hover_state);
        compositor.drag_handle_hover_state = Arc::clone(&self.app.state.drag_handle_hover_state);
        compositor.composer_visual_layout = Arc::clone(&self.app.state.composer_visual_layout);
        self.offscreen_tokens_seen = self.app.state.tokens_seen;
        self.offscreen = Some((compositor, runtime.surface));
        Ok(())
    }

    /// Real full-frame backend entries and successful local queue submissions.
    pub fn offscreen_work_counts(&self) -> (u64, u64) {
        self.offscreen_work
    }

    /// None identifies a renderer-unavailable turn, not an idle success.
    pub fn last_frame_submitted(&self) -> Option<bool> {
        self.offscreen.as_ref().map(|_| self.last_turn_submitted)
    }

    /// Run the real main-thread settle followed by the production frame turn.
    /// A missing renderer is explicit and never acknowledges a successful frame.
    /// Returns false on lock contention or deferred settle work, including
    /// input retained behind a busy scene lock.
    pub fn tick(&mut self) -> bool {
        self.last_turn_submitted = false;
        self.app.sync_design_tokens();
        if let Some((primary, secondaries)) = &self.display_fixture {
            self.app.sync_zone_assignments_for(primary, secondaries);
        } else {
            self.app.sync_zone_assignments();
        }
        // Settled as the production loop judges it: retained input still
        // waiting on the scene lock is unfinished work.
        let drain = self.app.settle_scene_work();
        let settled = !self.app.has_deferred_scene_work(&drain);
        let (width, height) = (
            self.app.state.config.window.width,
            self.app.state.config.window.height,
        );
        let Ok(state) = self.app.state.shared_state.try_lock() else {
            self.app.schedule_shared_scene_availability_wake();
            return false;
        };
        let scene_arc = Arc::clone(&state.scene);
        drop(state);
        let Ok(mut scene) = scene_arc.try_lock() else {
            let waiting_scene = Arc::clone(&scene_arc);
            self.app
                .state
                .wake
                .schedule_compositor_after_availability(move || {
                    drop(waiting_scene.blocking_lock());
                });
            return false;
        };
        let mut repaint = false;
        let (renderer, surface) = match self.offscreen.as_mut() {
            Some((renderer, surface)) => {
                repaint = renderer.set_safe_mode_overlay(
                    self.app
                        .state
                        .safe_mode_atomic
                        .load(std::sync::atomic::Ordering::Acquire),
                );
                if self.offscreen_tokens_seen != self.app.state.tokens_seen {
                    renderer.set_token_map(self.app.state.global_tokens.clone());
                    self.offscreen_tokens_seen = self.app.state.tokens_seen;
                    repaint = true;
                }
                (Some(renderer), Some(surface))
            }
            None => (None, None),
        };
        let turn = self.frame_turn.run_frame_turn(
            &mut scene,
            renderer,
            crate::pipeline::FrameTurnRequest {
                repaint,
                ..Default::default()
            },
            |_| (),
            |renderer, scene, needs_render| {
                let (telemetry, submitted) = if needs_render {
                    self.offscreen_work.0 += 1;
                    let (telemetry, submitted) = renderer.render_frame_headless_with_submission(
                        scene,
                        surface.expect("attached renderer owns a surface"),
                    );
                    self.offscreen_work.1 += u64::from(submitted);
                    (Some(telemetry), submitted)
                } else {
                    (None, false)
                };
                crate::pipeline::FrameTurnBuild {
                    work: telemetry,
                    submitted,
                }
            },
        );
        self.last_turn_submitted = turn.submitted;
        if turn.work.is_none() {
            // GPU-free callers need approximate notification geometry, but do
            // not claim render/snapshot/presentation evidence.
            let layout = self
                .app
                .state
                .display_targets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .layout
                .clone();
            tze_hud_compositor::renderer::hit_regions::populate_notification_hit_regions_on(
                &mut scene,
                &layout,
                width as f32,
                height as f32,
                &std::collections::HashMap::new(),
            );
        }
        let wakes = scene.take_notification_wait_notifiers();
        drop(scene);
        for wake in wakes {
            wake.notify();
        }
        // The turn after a frame observes what it drew (the press guard); a
        // busy scene leaves that observation, and so the turn, unsettled.
        self.app.settle_press_guard();
        settled && !self.app.state.press_guard.observation_due
    }

    /// Press and release the primary pointer button at display `(x, y)`
    /// through the winit pointer path.
    pub fn click(&mut self, x: f32, y: f32) {
        self.app.state.cursor_x = x;
        self.app.state.cursor_y = y;
        self.app.enqueue_pointer_event(PointerEventKind::Down);
        self.app.enqueue_pointer_event(PointerEventKind::Up);
    }

    /// Type `text` key by key the way the winit keyboard handler delivers it:
    /// key-down, character (for a character key), key-up. Space is a named
    /// key in winit, so it carries no character event.
    pub fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == ' ' {
                self.press_named_key("Space");
                continue;
            }
            let key_code = match ch {
                'a'..='z' | 'A'..='Z' => format!("Key{}", ch.to_ascii_uppercase()),
                '0'..='9' => format!("Digit{ch}"),
                _ => "Unidentified".to_string(),
            };
            self.press_key(&key_code, &ch.to_string(), Some(&ch.to_string()));
        }
    }

    /// Type `text` into the open control panel, key by key, through the
    /// window's panel key routing (the panel takes keys before agents do).
    pub fn panel_type(&mut self, text: &str) {
        use winit::keyboard::{Key, NamedKey, NativeKeyCode, PhysicalKey};
        let physical = PhysicalKey::Unidentified(NativeKeyCode::Unidentified);
        for ch in text.chars() {
            let logical = if ch == ' ' {
                Key::Named(NamedKey::Space)
            } else {
                Key::Character(ch.to_string().into())
            };
            assert!(
                self.app
                    .panel_key_input(None, physical, &logical, None, true),
                "the panel is open"
            );
            self.app
                .panel_key_input(None, physical, &logical, None, false);
        }
    }

    /// Press a named key (`Enter`, `Escape`, `Backspace`, `ArrowDown`...) on
    /// the open control panel.
    pub fn panel_press(&mut self, key: &str) {
        use winit::keyboard::{Key, NamedKey, NativeKeyCode, PhysicalKey};
        let named = match key {
            "Enter" => NamedKey::Enter,
            "Escape" => NamedKey::Escape,
            "Backspace" => NamedKey::Backspace,
            "ArrowDown" => NamedKey::ArrowDown,
            "ArrowUp" => NamedKey::ArrowUp,
            "ArrowLeft" => NamedKey::ArrowLeft,
            "ArrowRight" => NamedKey::ArrowRight,
            "Delete" => NamedKey::Delete,
            "Tab" => NamedKey::Tab,
            other => panic!("panel_press: unsupported key {other}"),
        };
        let physical = PhysicalKey::Unidentified(NativeKeyCode::Unidentified);
        let logical = Key::Named(named);
        assert!(
            self.app
                .panel_key_input(None, physical, &logical, None, true),
            "the panel is open"
        );
        self.app
            .panel_key_input(None, physical, &logical, None, false);
    }

    /// Press and release a named key (e.g. `"Enter"`), as winit delivers it.
    pub fn press_named_key(&mut self, key: &str) {
        self.press_key(key, key, None);
    }

    fn press_key(&mut self, key_code: &str, key: &str, character: Option<&str>) {
        let timestamp_mono_us = MonoUs(super::nanoseconds_since_start() / 1_000);
        self.app.dispatch_key_down_event(&RawKeyDownEvent {
            key_code: key_code.to_string(),
            key: key.to_string(),
            modifiers: KeyboardModifiers::NONE,
            repeat: false,
            timestamp_mono_us,
        });
        if let Some(character) = character {
            self.app.dispatch_character_event(&RawCharacterEvent {
                character: character.to_string(),
                timestamp_mono_us,
            });
        }
        self.app.dispatch_key_up_event(&RawKeyUpEvent {
            key_code: key_code.to_string(),
            key: key.to_string(),
            modifiers: KeyboardModifiers::NONE,
            timestamp_mono_us,
        });
    }

    /// Read the scene for a geometry lookup. Runtime tasks (an availability
    /// waiter, a session handler) may hold the lock for a moment; wait that
    /// out instead of reporting a target missing.
    fn with_scene<T>(
        &self,
        read: impl FnOnce(&tze_hud_scene::graph::SceneGraph) -> T,
    ) -> Option<T> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Ok(state) = self.app.state.shared_state.try_lock()
                && let Ok(scene) = state.scene.try_lock()
            {
                return Some(read(&scene));
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::yield_now();
        }
    }

    /// Display-space center of the first composer input region on screen
    /// (a portal armed with `expects_reply`), where a user would click to type.
    pub fn composer_center(&self) -> Option<(f32, f32)> {
        self.with_scene(|scene| {
            scene.tiles.values().find_map(|tile| {
                let mut stack: Vec<SceneId> = tile.root_node.into_iter().collect();
                while let Some(id) = stack.pop() {
                    let node = scene.nodes.get(&id)?;
                    if let NodeData::HitRegion(region) = &node.data
                        && region.accepts_composer_input
                    {
                        return Some((
                            tile.bounds.x + region.bounds.x + region.bounds.width / 2.0,
                            tile.bounds.y + region.bounds.y + region.bounds.height / 2.0,
                        ));
                    }
                    stack.extend(node.children.iter().copied());
                }
                None
            })
        })?
    }

    /// Display-space center of the on-screen notification action button
    /// whose callback is `callback_id`, as hit-tested after the last
    /// [`Self::tick`].
    pub fn notification_action_center(&self, callback_id: &str) -> Option<(f32, f32)> {
        self.notification_target_center(|a| a.callback_id.as_deref() == Some(callback_id))
    }

    /// Display center of a live link, using the compositor's exact action index.
    pub fn notification_link_center(&self, url: &str) -> Option<(f32, f32)> {
        self.notification_target_center(|a| a.url.as_deref() == Some(url))
    }

    fn notification_target_center(
        &self,
        matches: impl Fn(&tze_hud_scene::NotificationAction) -> bool,
    ) -> Option<(f32, f32)> {
        self.with_scene(|scene| {
            scene
                .overlay
                .zone_hit_regions
                .iter()
                .find(|region| scene.notification_action(region).is_some_and(&matches))
                .map(|region| {
                    (
                        region.bounds.x + region.bounds.width / 2.0,
                        region.bounds.y + region.bounds.height / 2.0,
                    )
                })
        })?
    }

    /// Widget instance `name`'s geometry override and auto-clear limit.
    pub fn widget_layout(
        &self,
        name: &str,
    ) -> Option<(Option<tze_hud_scene::types::GeometryPolicy>, Option<u64>)> {
        let scene = self.scene();
        scene
            .widget_registry
            .instances
            .get(name)
            .map(|i| (i.geometry_override, i.auto_clear_ms))
    }

    /// Stored deadlines of this publisher's active and accepted scheduled
    /// publications. This reads admission state without deriving new expiry.
    pub fn zone_publication_deadlines(&self, zone: &str, namespace: &str) -> Vec<Option<u64>> {
        let scene = self.scene();
        let mut deadlines: Vec<_> = scene
            .zone_registry
            .active_publishes
            .get(zone)
            .into_iter()
            .flatten()
            .filter(|publication| publication.publisher_namespace == namespace)
            .map(|publication| publication.expires_at_wall_us)
            .chain(scene.scheduled_zone_publication_expiries(zone, namespace))
            .collect();
        deadlines.sort_unstable();
        deadlines
    }

    /// Number of publications currently shown in zone `zone`.
    pub fn zone_publication_count(&self, zone: &str) -> usize {
        let scene = self.scene();
        scene
            .zone_registry
            .active_publishes
            .get(zone)
            .map_or(0, Vec::len)
    }

    /// Dismiss every tile on screen the way the hover close button does, through
    /// the runtime's real viewer-dismiss entry (`PortalDriver::viewer_dismiss_tile`).
    pub fn viewer_dismiss_all_tiles(&mut self) {
        let mut scene = self.scene();
        let ids: Vec<SceneId> = scene.tiles.keys().copied().collect();
        for id in ids {
            self.app
                .state
                .portal_projection_driver
                .viewer_dismiss_tile(&mut scene, id);
        }
    }

    /// Everything is owner-hidden (Hide all).
    pub fn all_hidden(&self) -> bool {
        self.scene().owner_hidden() == tze_hud_scene::graph::SurfaceClasses::ALL
    }

    /// Number of tiles on screen (portal surfaces included).
    pub fn tile_count(&self) -> usize {
        self.scene().tiles.len()
    }

    /// Build a harness around an inert-but-real `WinitApp`.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        HeadlessEventLoopHarness {
            app: WinitApp {
                state: WindowedRuntimeState::new_headless(),
            },
            frame_turn: crate::pipeline::FrameTurnState::default(),
            offscreen: None,
            last_turn_submitted: false,
            offscreen_work: (0, 0),
            offscreen_tokens_seen: 0,
            mcp_addr: None,
            grpc_addr: None,
            clipboard: Arc::default(),
            effects: Arc::default(),
            pairing: None,
            display_fixture: None,
        }
    }

    /// Drive the actual topology adoption/status publisher without OS windows.
    #[cfg(test)]
    pub(crate) fn publish_display_fixture(
        &mut self,
        configured: std::collections::HashMap<String, String>,
        names: &[&str],
    ) {
        let primary = super::displays::MonitorSpec {
            name: names[0].into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
        };
        self.app.state.zone_displays = configured;
        self.app
            .adopt_display_keys(super::displays::key_monitors(names, &[]), &[]);
        self.app.publish_displays(&primary);
    }

    /// Install a single-tab scene containing a focused composer region and seed
    /// the lock-free `active_tab_mirror`, so the keyboard-drain path routes
    /// character events into the composer draft. Returns the active tab id.
    ///
    /// Focus is acquired through the same production path a pointer-down would
    /// use: [`InputProcessor::process_with_focus`] on the harness's *own*
    /// `input_processor` and `focus_manager` (the exact fields the drain later
    /// reads).
    #[cfg(test)]
    pub(super) fn focus_composer(&mut self) -> SceneId {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 800.0, 600.0),
                1,
            )
            .unwrap();
        let composer_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: composer_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 800.0, 60.0),
                        interaction_id: "composer-input".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        accepts_composer_input: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        // Focus the composer via the production pointer-down focus path, using
        // the harness's own input_processor + focus_manager (disjoint field
        // borrows on `state`).
        self.app.state.focus_manager.add_tab(tab_id);
        let pointer = PointerEvent {
            x: 10.0,
            y: 10.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        self.app.state.input_processor.process_with_focus(
            &pointer,
            &mut scene,
            &mut self.app.state.focus_manager,
            tab_id,
        );
        assert!(
            self.app.state.input_processor.is_composer_active(),
            "composer must be active after focusing the composer region"
        );

        // Install the focused scene into shared_state and seed the mirror, the
        // way the post-apply_batch refresh does in production. Sync test → no
        // Tokio runtime is entered, so blocking_lock is safe.
        {
            let shared = self.app.state.shared_state.blocking_lock();
            *shared.scene.blocking_lock() = scene;
        }
        *self.app.state.active_tab_mirror.lock().unwrap() = Some(tab_id);
        tab_id
    }

    /// Enqueue a synthetic keyboard event onto the runtime's pending queue,
    /// exactly as the winit event handler does when a dispatch is deferred.
    #[cfg(test)]
    pub(super) fn enqueue(&mut self, event: PendingKeyboardEvent) {
        self.app.state.pending_keyboard_events.push_back(event);
    }

    /// Number of events still pending (undrained).
    #[cfg(test)]
    pub(super) fn pending_len(&self) -> usize {
        self.app.state.pending_keyboard_events.len()
    }

    /// Peek the front pending event (for FIFO-ordering assertions).
    #[cfg(test)]
    pub(super) fn front_pending(&self) -> Option<&PendingKeyboardEvent> {
        self.app.state.pending_keyboard_events.front()
    }

    /// Run the genuine production drain over the pending queue.
    #[cfg(test)]
    pub(super) fn drain(&mut self) {
        self.app.sync_answer_focus();
        self.app.drain_pending_keyboard_events();
    }

    /// Exercise the exact production completion-wake path that
    /// `about_to_wait` uses after a busy shared-scene observation.
    #[cfg(test)]
    pub(super) fn schedule_shared_scene_availability_wake(&self) {
        self.app.schedule_shared_scene_availability_wake();
    }

    /// Current composer draft text, if a composer is active.
    pub fn composer_draft(&self) -> Option<String> {
        self.app
            .state
            .input_processor
            .composer_draft_snapshot()
            .map(|(text, ..)| text)
    }

    /// A clone of the lock-free `active_tab_mirror` handle, so a test can hold
    /// its guard to simulate mirror contention.
    #[cfg(test)]
    pub(super) fn active_tab_mirror(&self) -> Arc<std::sync::Mutex<Option<SceneId>>> {
        Arc::clone(&self.app.state.active_tab_mirror)
    }

    /// A clone of the `shared_state` handle, so a test can hold its guard to
    /// simulate scene/shared-state lock contention (the busy-defer path).
    #[cfg(test)]
    pub(super) fn shared_state(
        &self,
    ) -> Arc<tokio::sync::Mutex<tze_hud_protocol::session::SharedState>> {
        Arc::clone(&self.app.state.shared_state)
    }
}

const BUSY: &str = "scene stayed locked by another task for 30 s";

/// How long a scene query waits out a network task holding the scene.
const SCENE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

impl Drop for HeadlessEventLoopHarness {
    /// Stop the MCP accept loop with the harness.
    fn drop(&mut self) {
        self.app
            .state
            .shutdown
            .trigger(crate::threads::ShutdownReason::Clean);
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    mod hold;
    mod press_guard;
    mod question_displays;
    use tze_hud_input::{KeyboardModifiers, RawCharacterEvent, RawKeyDownEvent, RawKeyUpEvent};
    use tze_hud_scene::MonoUs;

    #[tokio::test]
    async fn production_turn_skips_idle_and_renders_scene_changes_and_animation() {
        let _gpu = crate::test_support::lock_headless_runtime().await;
        let mut cfg = super::super::WindowedConfig::default();
        cfg.window.width = 320;
        cfg.window.height = 240;
        cfg.config_toml = Some(
            r#"
            [[tabs]]
            name = "gate"
            [[tabs.widgets]]
            instance_id = "gate-progress"
            widget_type = "progress-bar"
        "#
            .into(),
        );
        let mut harness = HeadlessEventLoopHarness::with_network(
            cfg,
            Arc::new(tze_hud_scene::TestClock::new(1_000)),
        )
        .await
        .unwrap();
        assert!(harness.tick());
        assert_eq!(harness.last_frame_submitted(), None);
        assert_eq!(harness.frame_turn.last_rendered(), (u64::MAX, u64::MAX));
        harness
            .enable_offscreen_rendering()
            .await
            .expect("real offscreen backend");
        assert!(harness.tick());
        assert_eq!(harness.last_frame_submitted(), Some(true));
        assert_eq!(harness.offscreen_work_counts(), (1, 1));
        assert!(
            harness
                .offscreen
                .as_mut()
                .unwrap()
                .0
                .take_work_counts()
                .is_some()
        );
        for _ in 0..3 {
            assert!(harness.tick());
            assert_eq!(harness.last_frame_submitted(), Some(false));
            assert_eq!(
                harness.offscreen_work_counts(),
                (1, 1),
                "settled empty turns do no build or submit"
            );
            assert!(
                harness
                    .offscreen
                    .as_mut()
                    .unwrap()
                    .0
                    .take_work_counts()
                    .is_none()
            );
        }
        harness.enable_offscreen_rendering().await.unwrap();
        assert!(harness.tick());
        assert_eq!(
            harness.offscreen_work_counts(),
            (1, 1),
            "repeated enable retains the existing backend and its ledger"
        );
        {
            let mut scene = harness.scene();
            scene.zone_registry = tze_hud_scene::ZoneRegistry::with_defaults();
            scene
                .zone_registry
                .zones
                .get_mut("subtitle")
                .unwrap()
                .rendering_policy
                .transition_in_ms = Some(0);
            scene
                .publish_to_zone(
                    "subtitle",
                    tze_hud_scene::ZoneContent::StreamText("held".into()),
                    "gate",
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
        assert!(harness.tick());
        assert_eq!(
            harness.offscreen_work_counts(),
            (2, 2),
            "a real publication builds and submits"
        );
        harness
            .offscreen
            .as_mut()
            .unwrap()
            .0
            .take_work_counts()
            .unwrap();
        for _ in 0..3 {
            assert!(harness.tick());
            assert_eq!(
                harness.offscreen_work_counts(),
                (2, 2),
                "held content does no further build or submit"
            );
            assert!(
                harness
                    .offscreen
                    .as_mut()
                    .unwrap()
                    .0
                    .take_work_counts()
                    .is_none()
            );
        }
        {
            let mut scene = harness.scene();
            scene
                .zone_registry
                .zones
                .get_mut("subtitle")
                .unwrap()
                .rendering_policy
                .transition_out_ms = Some(0);
            scene.clear_zone_for_publisher("subtitle", "gate").unwrap();
        }
        assert!(harness.tick());
        {
            let mut scene = harness.scene();
            // Long content transition is an input fixture, not a gate timeout.
            scene
                .zone_registry
                .zones
                .get_mut("subtitle")
                .unwrap()
                .rendering_policy
                .transition_in_ms = Some(60_000);
            scene
                .publish_to_zone(
                    "subtitle",
                    tze_hud_scene::ZoneContent::StreamText("animation".into()),
                    "gate",
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
        assert!(harness.tick());
        let versions = {
            let scene = harness.scene();
            (scene.version, scene.geometry_epoch)
        };
        assert_eq!(harness.frame_turn.last_rendered(), versions);
        let before = harness.offscreen_work_counts();
        assert!(harness.tick());
        assert_eq!(
            harness.frame_turn.last_rendered(),
            versions,
            "animation renders with both version ledgers pinned"
        );
        assert_eq!(
            harness.offscreen_work_counts(),
            (before.0 + 1, before.1 + 1)
        );
        assert!(
            harness
                .offscreen
                .as_mut()
                .unwrap()
                .0
                .take_work_counts()
                .is_some()
        );
        {
            let mut scene = harness.scene();
            scene.clear_zone_for_publisher("subtitle", "gate").unwrap();
        }
        assert!(harness.tick());
        assert!(harness.tick());
        assert_eq!(harness.last_frame_submitted(), Some(false));
        let before = harness.offscreen_work_counts();
        let old_tokens = harness.offscreen.as_ref().unwrap().0.token_map.clone();
        let appearance = Arc::clone(harness.app.state.appearance.as_ref().unwrap());
        appearance
            .apply(std::collections::HashMap::from([(
                "theme".into(),
                "blueprint".into(),
            )]))
            .await
            .unwrap();
        assert!(harness.tick());
        assert_eq!(
            harness.offscreen_work_counts(),
            (before.0 + 1, before.1 + 1)
        );
        assert_eq!(
            harness.offscreen.as_ref().unwrap().0.token_map,
            harness.app.state.global_tokens
        );
        assert_ne!(harness.offscreen.as_ref().unwrap().0.token_map, old_tokens);
        assert!(harness.tick());
        assert_eq!(
            harness.offscreen_work_counts(),
            (before.0 + 1, before.1 + 1),
            "live tokens repaint once then settle"
        );
        {
            let mut scene = harness.scene();
            scene
                .publish_to_widget("gate-progress", Default::default(), "gate", None, 0, None)
                .unwrap();
        }
        assert!(harness.tick());
        let renderer = harness
            .offscreen
            .as_ref()
            .unwrap()
            .0
            .widget_renderer()
            .unwrap();
        assert!(
            renderer.texture_entry("gate-progress").is_some(),
            "configured SVGs render on the actual harness scene"
        );
        assert!(renderer.raster_count("gate-progress") > 0);
    }

    /// Real frame builds and runtime accounting; only offscreen readback and
    /// swapchain acquire/submit/present are synthetic. GPU pixels and PNG have
    /// their own dedicated compositor/operator gates.
    struct CaptureRenderHarness {
        events: HeadlessEventLoopHarness,
        compositor: tze_hud_compositor::Compositor,
        frames: super::super::frame_work::WindowedFrameWork,
        pending_texture: bool,
        size: (u32, u32),
    }

    impl CaptureRenderHarness {
        /// A networked runtime over a software compositor at the window size.
        async fn boot(
            cfg: super::super::WindowedConfig,
            clock: Arc<tze_hud_scene::clock::TestClock>,
        ) -> Self {
            let size = (cfg.window.width, cfg.window.height);
            let mut compositor = tze_hud_compositor::Compositor::new_headless(size.0, size.1)
                .await
                .expect("software compositor for real frame builds");
            compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
            let events = HeadlessEventLoopHarness::with_network(cfg, clock)
                .await
                .unwrap();
            let frames = super::super::frame_work::WindowedFrameWork::new(
                events.app.state.wake.clone(),
                events.app.state.frame_ready_tx.as_ref().unwrap().clone(),
            );
            Self {
                events,
                compositor,
                frames,
                pending_texture: false,
                size,
            }
        }

        /// Fades run on the wall clock; pace turns like frames until idle.
        fn settle(&mut self, clock: &tze_hud_scene::clock::TestClock) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while self.turn(true) {
                assert!(std::time::Instant::now() < deadline, "portal never settled");
                clock.advance(16);
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
        }

        fn turn(&mut self, primary_succeeds: bool) -> bool {
            self.turn_with_present(primary_succeeds, true)
        }

        fn turn_with_present(&mut self, primary_succeeds: bool, encoding_complete: bool) -> bool {
            use super::super::frame_work::RenderTriggers;
            use tze_hud_compositor::renderer::frame::WindowedPresentOutcome;

            assert!(self.events.tick());
            let mut scene = self.events.scene();
            self.compositor.prime_markdown_cache(&scene);
            self.compositor.prime_truncation_cache(&scene);
            let prepared = self.frames.prepare(
                &mut self.compositor,
                &mut scene,
                self.size,
                &self.events.app.state.capture_inbox,
                RenderTriggers::default(),
                |needs_render| {
                    super::super::displays::frame_plan(
                        needs_render,
                        std::iter::empty(),
                        std::time::Instant::now(),
                    )
                },
            );
            let versions = (scene.version, scene.geometry_epoch);
            // The real post-render pass (`refresh_interaction_hit_regions_after_render`)
            // in place of `tick`'s GPU-free notification-only approximation.
            self.compositor.populate_zone_hit_regions(
                &mut scene,
                self.size.0 as f32,
                self.size.1 as f32,
            );
            drop(scene);
            if prepared.render.is_some() {
                let mut telemetry = tze_hud_telemetry::FrameTelemetry::new(1);
                telemetry.stage7_gpu_submit_us = u64::from(primary_succeeds);
                self.pending_texture = primary_succeeds;
                self.frames.record_primary_submission(
                    &WindowedPresentOutcome {
                        telemetry,
                        surface_acquired: primary_succeeds,
                        gpu_submitted: primary_succeeds,
                        signature_current: true,
                    },
                    versions,
                );
            }
            if let Some(capture) = prepared.capture {
                capture.finish_with(|build, display| {
                    assert!(display.is_none());
                    let (width, height) = build.size();
                    Ok(tze_hud_compositor::CapturedFrame {
                        width,
                        height,
                        rgba: [12, 34, 56, 255].repeat(width as usize * height as usize),
                    })
                });
            }
            self.events.app.maybe_present_frame_with(|| {
                Some(encoding_complete && std::mem::take(&mut self.pending_texture))
            });
            prepared.plan.full
        }
    }

    /// `/admin/status` `portals` and `idle_since_ms` read the state real frame
    /// builds and the production input path leave behind (T13).
    #[tokio::test]
    async fn admin_status_reports_rendered_portal_state() {
        use serde_json::Value;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tze_hud_portal::hub::{PortalKey, Publish};
        use tze_hud_scene::clock::TestClock;

        let _gpu = crate::test_support::lock_headless_runtime().await;
        let clock = Arc::new(TestClock::new(1_000));
        let mut cfg = super::super::WindowedConfig::default();
        cfg.window.width = 640;
        cfg.window.height = 480;
        cfg.agents = tze_hud_config::agents_file::AgentsFile::default()
            .with_agent("ops", "status-fixture-key", &["*", "admin"])
            .directory()
            .unwrap()
            .shared();
        let mut runtime = CaptureRenderHarness::boot(cfg, clock.clone()).await;
        let portals = runtime
            .events
            .app
            .state
            .portal_projection_driver
            .portal_handle();
        let key = PortalKey::new("ops", "probe");
        let publish = |lines: usize| {
            let text: Vec<String> = (0..lines).map(|i| format!("Line{i:02} output")).collect();
            portals
                .hub
                .lock()
                .unwrap()
                .publish(
                    &key,
                    Publish {
                        text: Some(text.join("\n")),
                        ..Default::default()
                    },
                    portals.clock.now_us(),
                )
                .expect("publish accepted");
        };
        let probe = crate::operator::surface_status::probe(
            Arc::clone(
                &runtime
                    .events
                    .app
                    .state
                    .shared_state
                    .try_lock()
                    .unwrap()
                    .scene,
            ),
            Arc::clone(&runtime.events.app.state.surface_ui),
            Arc::clone(&runtime.events.app.state.notification_link_opener.launches),
        );
        let portal = || probe.read().portals[0].clone();

        publish(40);
        runtime.settle(&clock);
        let at_tail = portal();
        assert_eq!(at_tail["s"], "portal:probe");
        assert_eq!(at_tail["output"]["at_tail"], true);
        // The live stream's cursor glyph follows the newest line.
        let last = at_tail["output"]["last_visible_line"].as_str().unwrap();
        assert!(last.starts_with("Line39 output"), "{last}");
        assert_eq!(
            at_tail["panes"]["unread_badge"],
            Value::Null,
            "no pill at the tail"
        );
        assert_eq!(at_tail["focus"], "none");
        assert_eq!(at_tail["input"]["at_tail"], true, "measured, empty INPUT");

        // Idle with content: no redraw, so the idle clock is never restarted.
        let counters = runtime.events.app.state.wake.counters().clone();
        let (idle, presents) = (counters.idle_since_ms(), counters.snapshot().presents);
        std::thread::sleep(std::time::Duration::from_millis(5));
        for _ in 0..3 {
            clock.advance(50);
            assert!(!runtime.turn(true), "an idle HUD renders nothing");
            let snapshot = probe.read();
            assert_eq!(snapshot.notifications, serde_json::json!([]));
            assert_eq!(snapshot.widgets, serde_json::json!([]));
            assert_eq!(snapshot.link_launches, 0);
        }
        assert_eq!(counters.snapshot().presents, presents);
        assert!(
            counters.idle_since_ms() > idle,
            "idle_since_ms keeps growing"
        );

        // Wheel up over OUTPUT through the production scroll path.
        let output = &at_tail["panes"]["output"];
        let center = |r: &Value, axis: &str, len: &str| {
            (r[axis].as_f64().unwrap() + r[len].as_f64().unwrap() / 2.0) as f32
        };
        runtime.events.app.state.cursor_x = center(output, "x", "w");
        runtime.events.app.state.cursor_y = center(output, "y", "h");
        runtime
            .events
            .app
            .enqueue_scroll_event(0.0, -400.0, tze_hud_input::WheelStep::Pixels);
        runtime.settle(&clock);
        assert!(counters.idle_since_ms() < 1_000, "the scroll presented");
        let scrolled = portal();
        assert_eq!(scrolled["output"]["at_tail"], false);
        let first = scrolled["output"]["first_visible_line"].clone();
        assert_ne!(first, at_tail["output"]["first_visible_line"]);
        assert_eq!(
            scrolled["panes"]["unread_badge"],
            Value::Null,
            "nothing unread: no pill even when scrolled back"
        );

        // New output while scrolled back keeps the reading position.
        publish(45);
        runtime.settle(&clock);
        let appended = portal();
        assert_eq!(appended["output"]["first_visible_line"], first);
        assert_eq!(appended["output"]["at_tail"], false);
        let unread = {
            let scene = runtime.events.scene();
            scene.tile_unread_count(*scene.tiles.keys().next().unwrap())
        };
        assert!(unread > 0, "output arrived while scrolled back");
        assert_eq!(appended["output"]["unread"], unread);
        let pill = &appended["panes"]["unread_badge"];
        assert!(
            pill["w"].as_f64().unwrap() > 0.0,
            "the counted output has its badge drawn"
        );
        for (pos, len) in [("x", "w"), ("y", "h")] {
            let (p, o) = (pill[pos].as_f64().unwrap(), output[pos].as_f64().unwrap());
            assert!(p >= o && p + pill[len].as_f64().unwrap() <= o + output[len].as_f64().unwrap());
        }

        // Click the composer, type: focus moves there and only the length shows.
        let (x, y) = runtime.events.composer_center().expect("composer");
        runtime.events.click(x, y);
        runtime.events.type_text("xyz");
        runtime.settle(&clock);

        let addr = runtime.events.mcp_addr();
        let status = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(
                    b"GET /admin/status HTTP/1.0\r\nAuthorization: Bearer status-fixture-key\r\n\r\n",
                )
                .await
                .unwrap();
            let mut raw = String::new();
            stream.read_to_string(&mut raw).await.unwrap();
            raw
        })
        .await
        .unwrap();
        assert!(
            !status.contains("xyz"),
            "the draft's text is never reported"
        );
        let body: Value = serde_json::from_str(status.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["notifications"], serde_json::json!([]));
        assert_eq!(body["widgets"], serde_json::json!([]));
        assert_eq!(body["link_launches"], 0);
        let typed = &body["portals"][0];
        assert_eq!(typed["focus"], "composer");
        assert_eq!(typed["input"]["draft_chars"], 3);
        assert_eq!(typed["output"]["first_visible_line"], first);
        assert!(body["idle_since_ms"].as_u64().is_some());
    }

    /// Escape and reading keys on real portals, through the production pointer, key and
    /// frame paths. R4.1: Esc collapses a draft selection, then leaves with the
    /// draft kept (drawn unfocused, its length reported, nothing sent); each
    /// composer keeps its own; a cleared portal drops its draft; refocus
    /// restores the caret and Enter sends the kept text once; safe mode keeps
    /// it. R4.2: with OUTPUT reading focus Home/End/Space/PageUp/PageDown scroll
    /// OUTPUT, an unmoved key changes nothing, and the caret keeps them as
    /// text keys.
    #[tokio::test]
    async fn portal_escape_keeps_draft_and_reading_keys_scroll_output() {
        use serde_json::Value;
        use tze_hud_portal::hub::{PortalKey, Publish};
        use tze_hud_scene::clock::TestClock;

        let _gpu = crate::test_support::lock_headless_runtime().await;
        let clock = Arc::new(TestClock::new(1_000));
        let mut cfg = super::super::WindowedConfig::default();
        cfg.window.width = 800;
        cfg.window.height = 600;
        let mut runtime = CaptureRenderHarness::boot(cfg, clock.clone()).await;
        let portals = runtime
            .events
            .app
            .state
            .portal_projection_driver
            .portal_handle();
        let (a, b) = (PortalKey::new("ops", "a"), PortalKey::new("ops", "b"));
        let publish = |key: &PortalKey, lines: usize| {
            let text: Vec<String> = (0..lines).map(|i| format!("Line{i:02} output")).collect();
            let mut hub = portals.hub.lock().unwrap();
            hub.publish(
                key,
                Publish {
                    text: Some(text.join("\n")),
                    ..Default::default()
                },
                portals.clock.now_us(),
            )
            .expect("publish accepted");
        };
        let probe = crate::operator::surface_status::probe(
            Arc::clone(
                &runtime
                    .events
                    .app
                    .state
                    .shared_state
                    .try_lock()
                    .unwrap()
                    .scene,
            ),
            Arc::clone(&runtime.events.app.state.surface_ui),
            Arc::clone(&runtime.events.app.state.notification_link_opener.launches),
        );
        let portal = |s: &str| {
            let all = probe.read().portals;
            all.as_array()
                .unwrap()
                .iter()
                .find(|p| p["s"] == s)
                .cloned()
                .unwrap_or_else(|| panic!("{s} in {all}"))
        };
        let centre = |r: &Value| {
            let f = |k: &str| r[k].as_f64().unwrap() as f32;
            (f("x") + f("w") / 2.0, f("y") + f("h") / 2.0)
        };
        let snapshot = |runtime: &CaptureRenderHarness| {
            let snap = runtime
                .events
                .app
                .state
                .input_processor
                .composer_draft_snapshot();
            snap.map(|(text, cursor, anchor, ..)| (text, cursor, anchor))
        };
        let key = |runtime: &mut CaptureRenderHarness, key: &str, shift: bool| {
            let modifiers = KeyboardModifiers {
                shift,
                ..KeyboardModifiers::NONE
            };
            let ts = MonoUs(super::super::nanoseconds_since_start() / 1_000);
            let (key_code, key) = (key.to_string(), key.to_string());
            runtime
                .events
                .app
                .dispatch_key_down_event(&RawKeyDownEvent {
                    key_code: key_code.clone(),
                    key: key.clone(),
                    modifiers,
                    repeat: false,
                    timestamp_mono_us: ts,
                });
            runtime.events.app.dispatch_key_up_event(&RawKeyUpEvent {
                key_code,
                key,
                modifiers,
                timestamp_mono_us: ts,
            });
        };
        let inputs = || {
            let mut hub = portals.hub.lock().unwrap();
            let batch = hub.poll_input("ops", &[], None, portals.clock.now_us());
            let ids: Vec<String> = batch.items.iter().map(|i| i.id.clone()).collect();
            hub.poll_input("ops", &ids, None, portals.clock.now_us());
            batch.items.into_iter().map(|i| i.text).collect::<Vec<_>>()
        };

        publish(&a, 40);
        runtime.settle(&clock);
        let tile_a = *runtime.events.scene().tiles.keys().next().unwrap();
        let composer_a = runtime.events.scene().overlay.tile_composer_nodes[&tile_a];
        let draft_a = |runtime: &CaptureRenderHarness| {
            let ip = &runtime.events.app.state.input_processor;
            let mut drafts = ip.composer_drafts();
            drafts.find_map(|(n, d)| (n == composer_a).then(|| d.text().to_owned()))
        };
        let (ax, ay) = centre(&portal("portal:a")["panes"]["composer"]);
        runtime.events.click(ax, ay);
        runtime.events.type_text("half");
        runtime.settle(&clock);
        assert_eq!(portal("portal:a")["focus"], "composer");

        // S2: a draft selection collapses first; text and focus stay.
        runtime
            .events
            .app
            .state
            .input_processor
            .route_pointer_selection_to_composer(1, 3);
        runtime.events.press_named_key("Escape");
        assert_eq!(snapshot(&runtime), Some(("half".into(), 3, 3)));

        // S1: one more Esc leaves; the draft is kept, drawn and reported, and
        // nothing reaches hud_input.
        runtime.events.press_named_key("Escape");
        runtime.settle(&clock);
        assert_eq!(snapshot(&runtime), None, "focus left the composer");
        let left = portal("portal:a");
        assert_eq!(
            (&left["focus"], &left["input"]["draft_chars"]),
            (&"none".into(), &4.into())
        );
        assert_eq!(
            runtime.events.app.state.kept_drafts_drawn,
            [(composer_a, "half".to_string())],
            "Q2: the kept draft is sent to the compositor to draw"
        );
        assert_eq!(inputs(), Vec::<String>::new());

        // S8 (Q4 = A): a second portal's draft never displaces the first.
        publish(&b, 3);
        runtime.settle(&clock);
        let (bx, by) = centre(&portal("portal:b")["panes"]["composer"]);
        runtime.events.click(bx, by);
        runtime.events.type_text("bb");
        runtime.events.press_named_key("Escape");
        runtime.settle(&clock);
        assert_eq!(portal("portal:a")["input"]["draft_chars"], 4);
        assert_eq!(portal("portal:b")["input"]["draft_chars"], 2);
        assert_eq!(runtime.events.app.state.kept_drafts_drawn.len(), 2);

        // S5: a cleared portal takes its kept draft; a later one starts empty.
        portals.hub.lock().unwrap().clear(&b).unwrap();
        runtime.settle(&clock);
        let kept: Vec<_> = runtime
            .events
            .app
            .state
            .input_processor
            .composer_drafts()
            .map(|(node, _)| node)
            .collect();
        assert_eq!(kept, [composer_a]);
        assert_eq!(runtime.events.app.state.kept_drafts_drawn.len(), 1);
        publish(&b, 3);
        runtime.settle(&clock);
        assert_eq!(portal("portal:b")["input"]["draft_chars"], 0);
        portals.hub.lock().unwrap().clear(&b).unwrap();
        runtime.settle(&clock);

        // S1/S6: keyboard refocus restores text and caret (a click places the
        // caret where it lands); Enter sends it exactly once.
        runtime.events.press_named_key("Tab");
        assert_eq!(snapshot(&runtime), Some(("half".into(), 3, 3)));
        // R4.2 S4: with the caret in the composer, Home/End are caret keys.
        let output_before = runtime.events.scene().tile_scroll_offset_local(tile_a);
        runtime.events.press_named_key("Home");
        assert_eq!(snapshot(&runtime), Some(("half".into(), 0, 0)));
        runtime.events.press_named_key("End");
        assert_eq!(snapshot(&runtime), Some(("half".into(), 4, 4)));
        assert_eq!(
            runtime.events.scene().tile_scroll_offset_local(tile_a),
            output_before
        );
        runtime.events.press_named_key("Enter");
        runtime.settle(&clock);
        assert_eq!(inputs(), ["half"]);
        assert_eq!(inputs(), Vec::<String>::new(), "sent once");

        // S4: under safe mode Esc leaves nothing changed; the draft is kept.
        runtime.events.type_text("safe");
        let safe = Arc::clone(&runtime.events.app.state.safe_mode_atomic);
        safe.store(true, std::sync::atomic::Ordering::Release);
        runtime.events.press_named_key("Escape");
        safe.store(false, std::sync::atomic::Ordering::Release);
        assert_eq!(snapshot(&runtime).map(|s| s.0), Some("safe".into()));

        // R4.2 S1: double-click an OUTPUT word; the selection takes reading
        // focus from the caret, so Home and End scroll OUTPUT.
        let output = portal("portal:a")["panes"]["output"].clone();
        let (ox, oy) = (
            output["x"].as_f64().unwrap() as f32 + 24.0,
            centre(&output).1,
        );
        runtime.events.click(ox, oy);
        runtime.events.click(ox, oy);
        let selection = runtime
            .events
            .app
            .state
            .input_processor
            .output_reading
            .selection;
        assert!(selection.is_some(), "a word is selected");
        runtime.events.press_named_key("Home");
        runtime.settle(&clock);
        let top = portal("portal:a");
        assert!(
            top["output"]["first_visible_line"]
                .as_str()
                .unwrap()
                .starts_with("Line00")
        );
        assert_eq!(top["output"]["at_tail"], false);
        // S5: an unmoved key changes nothing, so nothing is drawn.
        let version = runtime.events.scene().version;
        runtime.events.press_named_key("Home");
        assert_eq!(runtime.events.scene().version, version);
        assert!(!runtime.turn(true), "Home at the top draws no frame");
        // S2: Space and Shift+Space page exactly like PageDown/PageUp.
        key(&mut runtime, "Space", false);
        assert_eq!(
            runtime.events.scene().tile_scroll_offset_local(tile_a).1,
            tze_hud_input::KEYBOARD_PAGE_SCROLL_PX
        );
        key(&mut runtime, "Space", true);
        assert_eq!(
            runtime.events.scene().tile_scroll_offset_local(tile_a).1,
            0.0
        );
        // PageDown/PageUp page OUTPUT the same way under a selection.
        runtime.events.press_named_key("PageDown");
        assert_eq!(
            runtime.events.scene().tile_scroll_offset_local(tile_a).1,
            tze_hud_input::KEYBOARD_PAGE_SCROLL_PX
        );
        runtime.events.press_named_key("PageUp");
        assert_eq!(
            runtime.events.scene().tile_scroll_offset_local(tile_a).1,
            0.0
        );
        runtime.events.press_named_key("End");
        runtime.settle(&clock);
        let tail = portal("portal:a");
        assert!(
            tail["output"]["last_visible_line"]
                .as_str()
                .unwrap()
                .starts_with("Line39")
        );
        assert_eq!(tail["output"]["at_tail"], true, "End is jump-to-latest");
        assert_eq!(
            runtime
                .events
                .app
                .state
                .input_processor
                .output_reading
                .selection,
            selection,
            "scrolling keeps the selection"
        );
        assert_eq!(draft_a(&runtime).as_deref(), Some("safe"), "no key typed");

        // R4.1 S3: Esc clears the OUTPUT selection first; the kept draft and
        // the keyboard focus stay as they were.
        let focus = portal("portal:a")["focus"].clone();
        runtime.events.press_named_key("Escape");
        let ip = &runtime.events.app.state.input_processor;
        assert!(ip.output_reading.selection.is_none());
        assert_eq!(draft_a(&runtime).as_deref(), Some("safe"));
        runtime.settle(&clock);
        assert_eq!(portal("portal:a")["focus"], focus);

        // The focused portal tile is reading focus too: Home still scrolls.
        assert_eq!(focus, "tile", "a press on OUTPUT focuses its portal tile");
        runtime.events.press_named_key("Home");
        runtime.settle(&clock);
        let first = portal("portal:a")["output"]["first_visible_line"].clone();
        assert!(first.as_str().unwrap().starts_with("Line00"), "{first}");
        assert_eq!(inputs(), Vec::<String>::new());
    }

    /// Portal reading (hud-ljhku.2) through the production pointer and key
    /// paths over a really shaped OUTPUT: links open through the link worker
    /// on release (rate limited, never on a drag or second click), drags and
    /// multi-clicks select, Ctrl+C and the copy button write plain text, Esc
    /// clears, Tab/Enter reach links, and the admin entry reports cursor,
    /// hint and selection length. Nothing reaches the agent's input.
    #[tokio::test]
    async fn portal_output_reading_links_selection_copy_and_cursor() {
        use tze_hud_input::KeyboardModifiers;
        use tze_hud_input::portal_reading::{OutputReading, ReadingStop};
        use tze_hud_portal::hub::{PortalKey, Publish};
        use tze_hud_scene::clock::TestClock;

        let _gpu = crate::test_support::lock_headless_runtime().await;
        let clock = Arc::new(TestClock::new(1_000));
        let mut cfg = super::super::WindowedConfig::default();
        cfg.window.width = 800;
        cfg.window.height = 600;
        let mut runtime = CaptureRenderHarness::boot(cfg, clock.clone()).await;
        let opened = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&opened);
        runtime.events.app.state.notification_link_opener.worker = Some(Arc::new(move |url| {
            sink.lock().unwrap().push(url);
            Ok(())
        }));
        let portals = runtime
            .events
            .app
            .state
            .portal_projection_driver
            .portal_handle();
        let key = PortalKey::new("ops", "read");
        let text = "Docs [alpha](https://alpha.example/a) and [beta](https://beta.example/b) or [gamma](https://gamma.example/c)\n\n\
                    Bare https://delta.example/d here and [plain](http://insecure.example)\n\n\
                    Line three of output\nLine four of output\nLine five of output\n\n\
                    ```sh\necho copied\n```";
        portals
            .hub
            .lock()
            .unwrap()
            .publish(
                &key,
                Publish {
                    text: Some(text.into()),
                    ..Default::default()
                },
                portals.clock.now_us(),
            )
            .expect("publish accepted");
        runtime.settle(&clock);
        let tile = *runtime.events.scene().tiles.keys().next().unwrap();
        let layout = runtime
            .events
            .app
            .state
            .input_processor
            .portal_output_layout(tile)
            .expect("shaped OUTPUT")
            .clone();
        let hrefs: Vec<&str> = layout.reading.links.iter().map(|l| &*l.href).collect();
        assert_eq!(
            hrefs,
            [
                "https://alpha.example/a",
                "https://beta.example/b",
                "https://gamma.example/c",
                "https://delta.example/d"
            ],
            "R2.1 S5/S6: the bare URL links; http:// stays plain"
        );
        // Screen point of a plain-text byte range's first rect centre.
        let at = |runtime: &CaptureRenderHarness, start: usize, end: usize, dx: f32| {
            let scene = runtime.events.scene();
            let b = scene.tiles[&tile].bounds;
            let scroll = scene.tile_scroll_offset_local(tile).1;
            let r = layout.range_rects(start, end)[0];
            (
                b.x + layout.viewport.x + r.x + r.width / 2.0 + dx,
                b.y + layout.viewport.y + r.y + r.height / 2.0 - scroll,
            )
        };
        let link_at = |runtime: &CaptureRenderHarness, i: usize| {
            let l = &layout.reading.links[i];
            at(runtime, l.start, l.end, 0.0)
        };
        let pointer =
            |runtime: &mut CaptureRenderHarness, (x, y): (f32, f32), kind: PointerEventKind| {
                runtime.events.app.state.cursor_x = x;
                runtime.events.app.state.cursor_y = y;
                runtime.events.app.enqueue_pointer_event(kind);
            };
        let launches = |runtime: &CaptureRenderHarness| {
            runtime
                .events
                .app
                .state
                .notification_link_opener
                .launches
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        let reader = |runtime: &CaptureRenderHarness| -> (usize, Option<String>) {
            let r = &runtime.events.app.state.input_processor.output_reading;
            (r.selection_chars(tile, &layout), r.link_hint(tile, &layout))
        };
        let multi_click =
            OutputReading::default().multi_click + std::time::Duration::from_millis(100);
        // Ctrl+C down and up, as a keyboard delivers it (a consumed key-down
        // swallows characters until its release).
        let ctrl_c = |runtime: &mut CaptureRenderHarness| {
            let ctrl = KeyboardModifiers {
                ctrl: true,
                ..KeyboardModifiers::NONE
            };
            let timestamp_mono_us = MonoUs(super::super::nanoseconds_since_start() / 1_000);
            runtime
                .events
                .app
                .dispatch_key_down_event(&RawKeyDownEvent {
                    key_code: "KeyC".into(),
                    key: "c".into(),
                    modifiers: ctrl,
                    repeat: false,
                    timestamp_mono_us,
                });
            runtime.events.app.dispatch_key_up_event(&RawKeyUpEvent {
                key_code: "KeyC".into(),
                key: "c".into(),
                modifiers: ctrl,
                timestamp_mono_us,
            });
        };

        // R2.1 S1: press and release within the threshold opens once on release.
        let alpha = link_at(&runtime, 0);
        pointer(&mut runtime, alpha, PointerEventKind::Down);
        assert_eq!(launches(&runtime), 0, "nothing opens on press");
        pointer(
            &mut runtime,
            (alpha.0 + 3.0, alpha.1 + 3.0),
            PointerEventKind::Move,
        );
        pointer(
            &mut runtime,
            (alpha.0 + 3.0, alpha.1 + 3.0),
            PointerEventKind::Up,
        );
        assert_eq!(launches(&runtime), 1);
        // R2.1 S4: the same href again within 2 s (not a double-click) does not launch.
        std::thread::sleep(multi_click);
        runtime.events.click(alpha.0, alpha.1);
        assert_eq!(launches(&runtime), 1, "same href within 2 s is refused");

        // R2.1 S3: a double-click opens once, on the first release, and selects nothing.
        let beta = link_at(&runtime, 1);
        runtime.events.click(beta.0, beta.1);
        runtime.events.click(beta.0, beta.1);
        assert_eq!(launches(&runtime), 2);
        assert_eq!(reader(&runtime).0, 0, "a link double-click selects nothing");

        // R2.1 S8 / R2.4 S1: hover a link: hand cursor, underline, host hint.
        let gamma = link_at(&runtime, 2);
        std::thread::sleep(multi_click);
        pointer(&mut runtime, gamma, PointerEventKind::Move);
        assert_eq!(
            runtime.events.app.state.reading_cursor,
            Some(tze_hud_input::PortalCursor::Pointer)
        );
        assert_eq!(
            reader(&runtime).1.as_deref(),
            Some("gamma.example"),
            "text differs from href"
        );
        let decor = runtime.events.scene().overlay.portal_reading[&tile].clone();
        assert_eq!(
            decor.link,
            Some((layout.reading.links[2].start, layout.reading.links[2].end))
        );
        assert_eq!(
            decor.hint.as_ref().map(|h| h.0.as_str()),
            Some("gamma.example")
        );

        // R2.1 S2: a press that moves past the threshold opens nothing and selects.
        pointer(&mut runtime, gamma, PointerEventKind::Down);
        pointer(
            &mut runtime,
            (gamma.0 + 40.0, gamma.1),
            PointerEventKind::Move,
        );
        pointer(
            &mut runtime,
            (gamma.0 + 40.0, gamma.1),
            PointerEventKind::Up,
        );
        assert_eq!(launches(&runtime), 2, "a drag from a link opens nothing");
        assert!(reader(&runtime).0 > 0, "and selects text");

        // R2.4 S1: I-beam over text, arrow over blank OUTPUT.
        let three = layout.content.find("Line three").unwrap();
        let text_point = at(&runtime, three, three + 4, 0.0);
        pointer(&mut runtime, text_point, PointerEventKind::Move);
        assert_eq!(
            runtime.events.app.state.reading_cursor,
            Some(tze_hud_input::PortalCursor::Text)
        );
        assert_eq!(reader(&runtime).1, None, "no hint without a hovered link");
        let blank_x = runtime.events.scene().tiles[&tile].bounds.x
            + layout.viewport.x
            + layout.viewport.width
            - 4.0;
        pointer(
            &mut runtime,
            (blank_x, text_point.1),
            PointerEventKind::Move,
        );
        assert_eq!(
            runtime.events.app.state.reading_cursor,
            Some(tze_hud_input::PortalCursor::Default)
        );

        // R2.2 S1: drag across three lines, Ctrl+C copies exactly that text.
        std::thread::sleep(multi_click);
        let five = layout.content.find("Line five").unwrap();
        let from = at(
            &runtime,
            three,
            three + 1,
            -(layout.range_rects(three, three + 1)[0].width / 2.0) + 1.0,
        );
        let to = at(&runtime, five + 3, five + 4, -1.0);
        pointer(&mut runtime, from, PointerEventKind::Down);
        pointer(&mut runtime, to, PointerEventKind::Move);
        pointer(&mut runtime, to, PointerEventKind::Up);
        let expected = &layout.content[three..five + 3];
        assert_eq!(reader(&runtime).0, expected.chars().count());
        assert!(
            !runtime.events.scene().tile_follow_tail_at_tail(tile)
                || layout.height <= layout.viewport.height,
            "R2.2 S3: a selection stops following the tail"
        );
        let writes = crate::clipboard::writes();
        ctrl_c(&mut runtime);
        assert!(crate::clipboard::writes() > writes);
        assert_eq!(
            crate::clipboard::LAST_TEXT
                .with(|t| t.borrow().clone())
                .as_deref(),
            Some(expected),
            "plain text of exactly the selection"
        );
        // R2.2 S4: Esc clears the selection.
        runtime.events.press_named_key("Escape");
        assert_eq!(reader(&runtime).0, 0);

        // R2.2 S2: double-click selects a word, triple-click the paragraph.
        let word = at(&runtime, three + 5, three + 6, 0.0);
        runtime.events.click(word.0, word.1);
        runtime.events.click(word.0, word.1);
        assert_eq!(reader(&runtime).0, "three".len());
        runtime.events.click(word.0, word.1);
        let paragraph = layout.paragraph_at(three);
        assert_eq!(
            reader(&runtime).0,
            layout.content[paragraph.0..paragraph.1].chars().count()
        );
        runtime.events.press_named_key("Escape");

        // R2.3 S1: hover the block, press its copy button: verbatim copy, "Copied".
        std::thread::sleep(multi_click);
        let (bs, be) = layout.reading.code_blocks[0];
        let block = at(&runtime, bs, bs + 1, 0.0);
        pointer(&mut runtime, block, PointerEventKind::Move);
        let button = runtime.events.scene().overlay.portal_reading[&tile]
            .copy_button
            .clone()
            .expect("hovering the block shows its copy button");
        let b = runtime.events.scene().tiles[&tile].bounds;
        let centre = (
            b.x + button.bounds.x + button.bounds.width / 2.0,
            b.y + button.bounds.y + button.bounds.height / 2.0,
        );
        pointer(&mut runtime, centre, PointerEventKind::Move);
        assert_eq!(
            runtime.events.app.state.reading_cursor,
            Some(tze_hud_input::PortalCursor::Pointer)
        );
        runtime.events.click(centre.0, centre.1);
        assert_eq!(
            crate::clipboard::LAST_TEXT
                .with(|t| t.borrow().clone())
                .as_deref(),
            Some(&layout.content[bs..be])
        );
        assert!(
            runtime.events.scene().overlay.portal_reading[&tile]
                .copy_button
                .as_ref()
                .is_some_and(|b| b.copied)
        );

        // R2.1 S7: Tab from the composer reaches the first link; Enter opens it
        // (alpha's 2 s window has passed, so it launches).
        std::thread::sleep(std::time::Duration::from_millis(1_200));
        let (x, y) = runtime.events.composer_center().expect("composer");
        runtime.events.click(x, y);
        runtime.events.press_named_key("Tab");
        assert_eq!(
            runtime
                .events
                .app
                .state
                .input_processor
                .output_reading
                .focus,
            Some((tile, ReadingStop::Link(0)))
        );
        runtime.events.press_named_key("Enter");
        assert_eq!(launches(&runtime), 3);
        // R3.2 S3: the focused link's hint pill shows what the press did in
        // place of its host; the admin hint stays the host, never the URL.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let decor = loop {
            runtime.settle(&clock);
            let decor = runtime.events.scene().overlay.portal_reading[&tile].clone();
            if decor.hint_feedback == Some(tze_hud_scene::LinkFeedbackState::Opened) {
                break decor;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pill never showed Opened"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(decor.hint.unwrap().0, "alpha.example");
        assert_eq!(
            runtime.events.scene().overlay.link_feedback,
            Some((
                tze_hud_scene::LinkFeedbackTarget::PortalLink {
                    tile,
                    href: "https://alpha.example/a".into()
                },
                tze_hud_scene::LinkFeedbackState::Opened
            ))
        );
        assert_eq!(
            runtime
                .events
                .app
                .state
                .surface_ui
                .lock()
                .unwrap()
                .link_feedback,
            Some("opened")
        );
        // R4.2 S3: Space on a focused link activates it; OUTPUT
        // reading focus does not turn it into a page scroll.
        runtime.events.press_named_key("Tab");
        runtime.events.press_named_key("Tab");
        let offset = runtime.events.scene().tile_scroll_offset_local(tile);
        runtime.events.press_named_key("Space");
        assert_eq!(launches(&runtime), 4, "Space opened gamma");
        assert_eq!(
            runtime.events.scene().tile_scroll_offset_local(tile),
            offset
        );
        // PageDown/PageUp page OUTPUT and keep the focused link.
        for page in ["PageDown", "PageUp"] {
            runtime.events.press_named_key(page);
            assert_eq!(
                runtime
                    .events
                    .app
                    .state
                    .input_processor
                    .output_reading
                    .focus,
                Some((tile, ReadingStop::Link(2))),
                "{page} keeps reading focus"
            );
        }
        assert_eq!(launches(&runtime), 4, "paging opens nothing");

        // A stale OUTPUT selection never takes a composer Ctrl+C:
        // a primary press outside OUTPUT clears it, browser-style.
        std::thread::sleep(multi_click);
        pointer(&mut runtime, from, PointerEventKind::Down);
        pointer(&mut runtime, to, PointerEventKind::Move);
        pointer(&mut runtime, to, PointerEventKind::Up);
        assert!(reader(&runtime).0 > 0, "OUTPUT selected again");
        runtime.events.click(x, y);
        assert_eq!(reader(&runtime).0, 0, "a composer press clears it");
        assert_eq!(
            runtime
                .events
                .app
                .state
                .input_processor
                .output_reading
                .focus,
            None,
            "and leaves reading focus"
        );
        runtime.events.type_text("draft words");
        runtime
            .events
            .app
            .state
            .input_processor
            .route_pointer_selection_to_composer(6, 11);
        ctrl_c(&mut runtime);
        assert_eq!(
            crate::clipboard::LAST_TEXT
                .with(|t| t.borrow().clone())
                .as_deref(),
            Some("words"),
            "the composer's selection wins"
        );

        // Every activation went to the worker; nothing reached hud_input.
        for worker in runtime
            .events
            .app
            .state
            .notification_link_opener
            .workers
            .drain(..)
        {
            worker.join().unwrap();
        }
        // Each launch runs on its own worker thread, so arrival order varies.
        let mut opened = opened.lock().unwrap().clone();
        opened.sort();
        assert_eq!(
            opened,
            [
                "https://alpha.example/a",
                "https://alpha.example/a",
                "https://beta.example/b",
                "https://gamma.example/c"
            ]
        );
        let hub = portals.hub.lock().unwrap();
        assert_eq!(
            hub.get(&key).unwrap().input_bytes(),
            0,
            "no OUTPUT interaction reaches the agent"
        );
        assert!(runtime.events.scene().pending_actions.is_empty());
    }

    /// Moves the former compositor build-only gate onto the runtime's actual
    /// capture drain, idle ledger, submission counters and main-thread present
    /// consumer. A failed ordinary acquire exposes capture advancing the ledger
    /// even when copying the already-presented version would look harmless.
    #[tokio::test]
    async fn building_an_idle_frame_leaves_the_idle_gate_inputs_unchanged() {
        use crate::operator::screenshot::capture_channel;
        use tze_hud_scene::clock::TestClock;

        #[derive(Clone, Default)]
        struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let writer = sink.clone();
        let _trace = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .json()
                .with_max_level(tracing::Level::DEBUG)
                .finish(),
        );
        let _gpu = crate::test_support::lock_headless_runtime().await;
        let mut compositor = tze_hud_compositor::Compositor::new_headless(320, 200)
            .await
            .expect("software compositor for real frame builds");
        println!("capture_runtime_adapter={:?}", compositor.adapter_info());
        compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
        let clock = Arc::new(TestClock::new(1_000));
        let mut cfg = super::super::WindowedConfig::default();
        cfg.window.width = 320;
        cfg.window.height = 200;
        cfg.agents = tze_hud_config::agents_file::AgentsFile::default()
            .with_agent("fixture", "present-fixture-key", &["*"])
            .directory()
            .unwrap()
            .shared();
        let mut events = HeadlessEventLoopHarness::with_network(cfg, clock.clone())
            .await
            .unwrap();
        let queued = Arc::new(tokio::sync::Notify::new());
        let (endpoint, inbox) = capture_channel({
            let queued = Arc::clone(&queued);
            let wake = events.app.state.wake.clone();
            move || {
                wake.compositor()
                    .notify(crate::idle_efficiency::RuntimeWakeupSource::OperatorCapture);
                queued.notify_one();
            }
        });
        events.app.state.capture_inbox = inbox;
        let frames = super::super::frame_work::WindowedFrameWork::new(
            events.app.state.wake.clone(),
            events.app.state.frame_ready_tx.as_ref().unwrap().clone(),
        );
        let mut runtime = CaptureRenderHarness {
            events,
            compositor,
            frames,
            pending_texture: false,
            size: (320, 200),
        };
        assert!(runtime.turn(true), "startup presents once");
        let versions = runtime.frames.last_rendered();
        let counters = runtime.events.app.state.wake.counters().snapshot();
        assert_eq!(
            (
                counters.surface_acquisitions,
                counters.gpu_queue_submissions,
                counters.presents
            ),
            (1, 1, 1)
        );

        for changed_scene in [false, true] {
            if changed_scene {
                let mut scene = runtime.events.scene();
                scene.create_tab("ordinary change", 0).unwrap();
                scene.bump_geometry_epoch();
            }
            let scene_versions = {
                let scene = runtime.events.scene();
                assert!(!runtime.compositor.has_inflight_animation(&scene));
                (scene.version, scene.geometry_epoch)
            };
            let capture = tokio::spawn({
                let endpoint = endpoint.clone();
                async move { endpoint.capture_frame(0).await }
            });
            // Endpoint enqueues before notifying; no yield/sleep or wall-clock
            // guess can race the production inbox drain.
            queued.notified().await;
            assert_eq!(runtime.turn(false), changed_scene);
            let (frame, permit) = capture.await.unwrap().unwrap();
            assert_eq!((frame.width, frame.height), (320, 200));
            assert_eq!(frame.rgba, [12, 34, 56, 255].repeat(320 * 200));
            drop(permit);
            assert_eq!(
                runtime.frames.last_rendered(),
                versions,
                "capture cannot consume an unpresented change"
            );
            assert_eq!(
                runtime.events.app.state.wake.counters().snapshot(),
                counters,
                "capture cannot acquire, submit or present the primary"
            );
            let scene = runtime.events.scene();
            assert_eq!(
                (scene.version, scene.geometry_epoch),
                scene_versions,
                "building a capture must not mutate idle inputs"
            );
            assert!(!runtime.compositor.has_inflight_animation(&scene));
        }
        assert!(
            runtime.turn(true),
            "ordinary change still needs its successful present"
        );
        let scene = runtime.events.scene();
        assert_eq!(
            runtime.frames.last_rendered(),
            (scene.version, scene.geometry_epoch)
        );
        drop(scene);
        let counters = runtime.events.app.state.wake.counters().snapshot();
        assert_eq!(
            (
                counters.surface_acquisitions,
                counters.gpu_queue_submissions,
                counters.presents
            ),
            (2, 2, 2)
        );
        assert!(
            !runtime.turn(true),
            "successful present restores static idle"
        );
        assert_eq!(
            runtime.events.app.state.wake.counters().snapshot(),
            counters
        );
        let versions = runtime.frames.last_rendered();
        *runtime.compositor.focus_ring_owner_state.lock().unwrap() =
            Some(tze_hud_compositor::renderer::FocusRingOwner {
                tab_id: SceneId::new(),
                tile_id: SceneId::new(),
                node_id: None,
            });
        assert!(
            runtime.turn(false),
            "local-only feedback requests an ordinary attempt"
        );
        assert_eq!(
            runtime.frames.last_rendered(),
            versions,
            "failed attempt does not acknowledge feedback"
        );
        assert!(
            runtime.turn(true),
            "consumed local-only feedback must retry until submission succeeds"
        );
        assert!(
            !runtime.turn(true),
            "the successful local-feedback retry returns to idle"
        );
        // A submitted image can be reused by a later encode before the main
        // thread presents it. If that encode is still busy, consuming the ready
        // signal must not discard the pending image's presentation obligation.
        runtime
            .events
            .scene()
            .create_tab("deferred present", 1)
            .unwrap();
        let before = runtime.events.app.state.wake.counters().snapshot();
        assert!(runtime.turn_with_present(true, false));
        assert!(runtime.pending_texture);
        assert_eq!(
            runtime.events.app.state.wake.counters().snapshot().presents,
            before.presents,
            "busy encode is not a successful present"
        );
        let wake = runtime.events.app.state.wake.clone();
        let compositor_before = wake.compositor().checkpoint();
        let main_work_before = wake.main_work_generation();
        wake.notify_main(crate::idle_efficiency::RuntimeWakeupSource::OperatorCapture);
        wake.take_main_source();
        let (availability, encoding) =
            tze_hud_compositor::WindowSurface::pending_encoding_for_test();
        let (other_window, other_encoding) =
            tze_hud_compositor::WindowSurface::pending_encoding_for_test();
        assert!(
            runtime
                .events
                .app
                .defer_present_until_available(other_window)
        );
        assert!(
            runtime
                .events
                .app
                .defer_present_until_available(availability.clone())
        );
        assert!(
            !runtime
                .events
                .app
                .defer_present_until_available(availability)
        );
        assert_ne!(
            wake.take_main_source(),
            crate::idle_efficiency::RuntimeWakeupSource::FrameReady
        );
        // A capture while the image is still encoding must leave its debt and
        // primary counters untouched. The native capture queue/build is real.
        let pending_counters = wake.counters().snapshot();
        let capture = tokio::spawn({
            let endpoint = endpoint.clone();
            async move { endpoint.capture_frame(0).await }
        });
        queued.notified().await;
        assert!(!runtime.turn_with_present(false, false));
        let (_, permit) = capture.await.unwrap().unwrap();
        drop(permit);
        assert!(runtime.events.app.state.deferred_present && runtime.pending_texture);
        assert_eq!(wake.counters().snapshot(), pending_counters);
        // A second window remains encoding; it must not block this completion.
        drop(encoding);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while wake.take_main_source() != crate::idle_efficiency::RuntimeWakeupSource::FrameReady {
            assert!(
                std::time::Instant::now() < deadline,
                "encoding completion must wake main"
            );
            std::thread::yield_now();
        }
        drop(other_encoding);
        assert_eq!(
            wake.compositor().checkpoint(),
            compositor_before + 1,
            "only the explicit OperatorCapture notification advances compositor generation"
        );
        assert_eq!(wake.main_work_generation(), main_work_before);
        assert!(
            !runtime.turn(true),
            "completion must not rebuild unchanged content"
        );
        assert_eq!(
            runtime.events.app.state.wake.counters().snapshot().presents,
            before.presents + 1,
            "deferred image presents without another submission or panel wake"
        );
        assert!(!runtime.pending_texture);
        let completed = runtime.events.app.state.wake.counters().snapshot();
        assert_eq!(
            completed.gpu_queue_submissions,
            before.gpu_queue_submissions + 1
        );
        assert!(!runtime.turn(true));
        assert_eq!(
            runtime.events.app.state.wake.counters().snapshot(),
            completed
        );

        // Explicit invalidation/no-pending retires deferred debt, while a newer
        // submission during presentation retains its own changed watch version.
        runtime
            .events
            .scene()
            .create_tab("invalidate pending", 2)
            .unwrap();
        assert!(runtime.turn_with_present(true, false));
        runtime.pending_texture = false;
        runtime.events.app.maybe_present_frame_with(|| None);
        assert!(!runtime.events.app.state.deferred_present);
        runtime
            .events
            .app
            .maybe_present_frame_with(|| panic!("empty means idle"));
        runtime.events.scene().create_tab("newer ready", 3).unwrap();
        assert!(runtime.turn_with_present(true, false));
        let frames = &mut runtime.frames;
        let versions = frames.last_rendered();
        runtime.events.app.maybe_present_frame_with(|| {
            let mut telemetry = tze_hud_telemetry::FrameTelemetry::new(2);
            telemetry.stage7_gpu_submit_us = 1;
            frames.record_primary_submission(
                &tze_hud_compositor::renderer::frame::WindowedPresentOutcome {
                    telemetry,
                    surface_acquired: false,
                    gpu_submitted: true,
                    signature_current: true,
                },
                versions,
            );
            Some(true)
        });
        assert!(
            runtime
                .events
                .app
                .state
                .frame_ready_rx
                .has_changed()
                .unwrap()
        );
        runtime.events.app.maybe_present_frame_with(|| None);
        assert!(
            !runtime
                .events
                .app
                .state
                .frame_ready_rx
                .has_changed()
                .unwrap()
        );

        // Secondary-only outcomes use the same consumer and independently
        // coalesced dependency, without ever counting a primary present.
        let primary_counters = wake.counters().snapshot();
        runtime
            .events
            .app
            .state
            .frame_ready_tx
            .as_ref()
            .unwrap()
            .send(true)
            .unwrap();
        let (secondary, encoding) = tze_hud_compositor::WindowSurface::pending_encoding_for_test();
        runtime
            .events
            .app
            .maybe_present_frame_with_secondaries(|| None, |_| (false, vec![secondary], vec![]));
        assert!(runtime.events.app.state.deferred_present);
        wake.notify_main(crate::idle_efficiency::RuntimeWakeupSource::OperatorCapture);
        wake.take_main_source();
        drop(encoding);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while wake.take_main_source() != crate::idle_efficiency::RuntimeWakeupSource::FrameReady {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        runtime
            .events
            .app
            .maybe_present_frame_with_secondaries(|| None, |_| (true, vec![], vec![]));
        assert!(!runtime.events.app.state.deferred_present);
        assert_eq!(wake.counters().snapshot(), primary_counters);
        let sender = runtime
            .events
            .app
            .state
            .frame_ready_tx
            .as_ref()
            .unwrap()
            .clone();
        sender.send(true).unwrap();
        runtime.events.app.maybe_present_frame_with_secondaries(
            || None,
            |_| {
                sender.send(true).unwrap();
                (true, vec![], vec![])
            },
        );
        assert!(
            runtime
                .events
                .app
                .state
                .frame_ready_rx
                .has_changed()
                .unwrap(),
            "new secondary ready during present remains owed to the next turn"
        );
        runtime.events.app.maybe_present_frame_with(|| None);
        assert_eq!(wake.counters().snapshot(), primary_counters);

        // The actual MCP server owns publication and emits the render wake.
        // Network I/O is loopback and credentials are a local fixture literal.
        async fn call(addr: SocketAddr, tool: &str, arguments: serde_json::Value) {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":tool,"arguments":arguments}})
            .to_string();
            let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
            conn.write_all(format!("POST / HTTP/1.0\r\nContent-Type: application/json\r\nAuthorization: Bearer present-fixture-key\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            let mut response = String::new();
            conn.read_to_string(&mut response).await.unwrap();
            let response: serde_json::Value =
                serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert!(response.get("error").is_none(), "{response}");
            assert_ne!(response["result"]["isError"], true, "{response}");
        }
        let addr = runtime.events.mcp_addr();
        {
            let mut scene = runtime.events.scene();
            let policy = &mut scene
                .zone_registry
                .zones
                .get_mut("notification-area")
                .unwrap()
                .rendering_policy;
            policy.transition_in_ms = None;
            policy.transition_out_ms = None;
        }
        // Retire the prior synthetic pending image before the ordinary network
        // cases. These cases start from a settled quiet scene with panel closed.
        runtime.pending_texture = false;
        assert!(!runtime.turn(true));
        for delay in [0, 10] {
            let checkpoint = wake.compositor().checkpoint();
            let mut presents = wake.counters().snapshot().presents;
            call(
                addr,
                "hud_publish",
                serde_json::json!({
                    "surface":"zone:notification-area","key":"held","ttl_ms":0,"delay_ms":delay,
                    "content":{"title":"held","body":"ordinary presentation"}
                }),
            )
            .await;
            assert!(
                wake.compositor().checkpoint() > checkpoint,
                "MCP publish must notify compositor"
            );
            assert!(!runtime.events.control_panel_open());
            if delay > 0 {
                // Scheduling changes scene.version and may render admission once.
                // The future publication must remain absent, then settle idle.
                runtime.turn(true);
                assert_eq!(
                    runtime.events.zone_publication_count("notification-area"),
                    0
                );
                assert!(
                    !runtime.turn(true),
                    "future publication settles back to idle"
                );
                presents = wake.counters().snapshot().presents;
                clock.advance(delay);
            }
            assert!(
                runtime.turn(true),
                "accepted immediate/due held content renders"
            );
            assert_eq!(wake.counters().snapshot().presents, presents + 1);
            assert_eq!(
                runtime
                    .events
                    .zone_publication_deadlines("notification-area", "fixture"),
                vec![None]
            );
            let settled = wake.counters().snapshot();
            assert!(!runtime.turn(true));
            assert_eq!(
                wake.counters().snapshot(),
                settled,
                "unchanged held card is quiescent"
            );
            call(
                addr,
                "hud_clear",
                serde_json::json!({"surface":"zone:notification-area","key":"held"}),
            )
            .await;
            assert!(runtime.turn(true));
            assert_eq!(
                runtime.events.zone_publication_count("notification-area"),
                0
            );
            assert!(!runtime.turn(true));
        }

        // The real sweep and capture owners emit one unambiguous identity.
        // Two due batches deliberately cannot claim that identity, even when
        // LatestWins leaves one record. No animation sleeps are needed here.
        for batches in [1, 2] {
            sink.0.lock().unwrap().clear();
            let (batch_ids, expected_version) = {
                use tze_hud_scene::types::{ZoneContent, ZoneRegistry};
                use tze_hud_scene::{Publication, Publisher};
                let mut scene = runtime.events.scene();
                scene.zone_registry = ZoneRegistry::with_defaults();
                scene
                    .zone_registry
                    .zones
                    .get_mut("subtitle")
                    .unwrap()
                    .rendering_policy
                    .transition_in_ms = None;
                let lease = scene.grant_lease("delayed", 120_000);
                let due = scene.now_wall_us() + 3_000_000;
                let mut ids = Vec::new();
                for _ in 0..batches {
                    ids.push(scene.schedule_publication(
                        due,
                        Publisher {
                            lease_id: Some(lease),
                            ..Publisher::namespace("delayed")
                        },
                        Publication::Zone {
                            zone_name: "subtitle".into(),
                            content: ZoneContent::StreamText("capture".into()),
                            merge_key: None,
                            expires_at_wall_us: Some(due + 20_000_000),
                            content_classification: None,
                            breakpoints: Vec::new(),
                            held: false,
                        },
                    ));
                }
                (ids, scene.version)
            };
            clock.advance(3_000);
            let counters = runtime.events.app.state.wake.counters().snapshot();
            let ledger = runtime.frames.last_rendered();
            let capture = tokio::spawn({
                let endpoint = endpoint.clone();
                async move { endpoint.capture_frame(0).await }
            });
            queued.notified().await;
            assert!(runtime.turn(false));
            let (frame, permit) = capture.await.unwrap().unwrap();
            assert_eq!((frame.width, frame.height), (320, 200));
            drop(permit);
            assert_eq!(runtime.frames.last_rendered(), ledger);
            assert_eq!(
                runtime.events.app.state.wake.counters().snapshot(),
                counters
            );
            let applied_version = runtime.events.scene().version;
            assert!(applied_version > expected_version);
            let trace: Vec<serde_json::Value> = String::from_utf8(sink.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|e| e["target"] == "tze_hud_delay_diagnostic")
                .map(|e| e["fields"].clone())
                .collect();
            let applied: Vec<_> = trace
                .iter()
                .filter(|e| e["event"] == "scheduled_activation")
                .collect();
            assert_eq!(applied.len(), batches as usize);
            for (event, id) in applied.iter().zip(batch_ids) {
                let (hi, lo) = id.as_uuid().as_u64_pair();
                assert_eq!(event["batch_hi"], hi);
                assert_eq!(event["batch_lo"], lo);
                assert_eq!(event["accepted"], true);
                assert_eq!(event["identity_valid"], batches == 1);
                assert_eq!(event["scene_version"], applied_version);
                assert_eq!(event["active_publications"], 1);
            }
            let begin = trace
                .iter()
                .position(|e| e["event"] == "capture_build_begin")
                .unwrap();
            let end = trace
                .iter()
                .position(|e| e["event"] == "capture_build_end")
                .unwrap();
            let rendered = trace
                .iter()
                .position(|e| e["event"] == "capture_render_end")
                .unwrap();
            assert!(begin < end && end < rendered);
            let sample = trace[begin + 1..end]
                .iter()
                .find(|e| e["event"] == "zone_text_sample")
                .unwrap();
            assert_eq!(sample["scene_version"], applied_version);
            assert_eq!(sample["opacity"], 1.0);
            assert_eq!(sample["transition_active"], false);
            let scene = runtime.events.scene();
            assert_eq!(
                sample["published_at_wall_us"],
                scene.zone_registry.active_publishes["subtitle"][0].published_at_wall_us
            );
            assert_eq!(trace[rendered]["success"], true);
        }
        {
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let release = Arc::new(std::sync::Mutex::new(release_rx));
            let shared = runtime.events.shared_state();
            runtime.events.app.state.notification_link_opener.worker = Some(Arc::new(move |url| {
                let state = shared
                    .try_lock()
                    .expect("worker owns no shared-state guard");
                assert!(state.scene.try_lock().is_ok(), "worker owns no scene guard");
                drop(state);
                entered_tx.send((url, std::thread::current().id())).unwrap();
                release.lock().unwrap().recv().unwrap();
                Ok(())
            }));
            {
                let mut scene = runtime.events.scene();
                let mut zone = tze_hud_scene::ZoneRegistry::with_defaults()
                    .zones
                    .remove("notification-area")
                    .unwrap();
                zone.geometry_policy = tze_hud_scene::GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.0,
                    width_pct: 1.0,
                    height_pct: 1.0,
                };
                scene.register_zone(zone);
                scene
                    .publish_to_zone(
                        "notification-area",
                        tze_hud_scene::ZoneContent::Notification(
                            tze_hud_scene::NotificationPayload {
                                text: "Blocked browser".into(),
                                actions: vec![tze_hud_scene::NotificationAction {
                                    copy: None,
                                    reply: None,
                                    label: "Open".into(),
                                    callback_id: None,
                                    url: Some("https://example.com/frame".into()),
                                    role: None,
                                }],
                                ..Default::default()
                            },
                        ),
                        "fixture",
                        None,
                        None,
                        None,
                    )
                    .unwrap();
            }
            assert!(runtime.events.tick());
            let point = runtime
                .events
                .notification_link_center("https://example.com/frame")
                .unwrap();
            runtime.events.click(point.0, point.1);
            runtime.events.drain();
            let (url, worker) = entered_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            assert_eq!(url, "https://example.com/frame");
            assert_ne!(worker, std::thread::current().id());
            // The worker is still waiting on release_rx. Drive the actual
            // pending-keyboard drain and observe local consumption before
            // releasing it, independently of the frame/scene progress below.
            assert!(runtime.events.scene().overlay.notification_focus.is_none());
            runtime.events.enqueue(key_down("Tab", "Tab", 2_000));
            runtime.events.drain();
            assert!(runtime.events.scene().overlay.notification_focus.is_some());
            assert_eq!(runtime.events.app.state.notification_keydowns.len(), 1);
            runtime.events.enqueue(key_up("Tab", "Tab", 2_100));
            runtime.events.drain();
            assert!(runtime.events.app.state.notification_keydowns.is_empty());
            assert!(runtime.events.app.state.pending_keyboard_events.is_empty());
            assert!(runtime.events.scene().pending_actions.is_empty());
            runtime.events.scene().bump_geometry_epoch();
            let before = runtime.events.app.state.wake.counters().snapshot();
            assert!(
                runtime.turn(true),
                "real frame preparation advances while opener waits"
            );
            let after = runtime.events.app.state.wake.counters().snapshot();
            assert_eq!(after.presents, before.presents + 1);
            assert_eq!(
                after.gpu_queue_submissions,
                before.gpu_queue_submissions + 1
            );
            assert!(runtime.events.scene().pending_actions.is_empty());
            release_tx.send(()).unwrap();
            for worker in runtime
                .events
                .app
                .state
                .notification_link_opener
                .workers
                .drain(..)
            {
                worker.join().unwrap();
            }
            runtime
                .events
                .scene()
                .clear_zone("notification-area")
                .unwrap();
            runtime.turn(true);
        }

        // The countdown's typed value and scene version stay unchanged as wall
        // time passes. Real capture builds may update its raster cache, but an
        // unsuccessful ordinary submission must retain the repaint obligation.
        runtime
            .compositor
            .init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
        {
            use tze_hud_scene::{GeometryPolicy, WidgetInstance};
            use tze_hud_widget::loader::{BundleScanResult, load_bundle_from_files};
            let tokens =
                tze_hud_config::tokens::resolve_tokens(&Default::default(), &Default::default());
            let BundleScanResult::Ok(bundle) = load_bundle_from_files(
                "countdown",
                &[
                    (
                        "widget.toml",
                        include_bytes!("../../../../assets/widget_bundles/countdown/widget.toml"),
                    ),
                    (
                        "card.svg",
                        include_bytes!("../../../../assets/widget_bundles/countdown/card.svg"),
                    ),
                ],
                &tokens,
            ) else {
                panic!("shipped countdown")
            };
            for (file, bytes) in bundle.svg_contents {
                runtime
                    .compositor
                    .widget_renderer_mut()
                    .unwrap()
                    .register_svg("countdown", &file, bytes);
            }
            let mut scene = runtime.events.scene();
            scene.clear_zone("subtitle").unwrap();
            // Retire prior test-only timed owners; only this section's 24h MCP
            // lease should influence its countdown-only idle observations.
            scene.leases.clear();
            let tab = scene.active_tab.unwrap();
            scene.widget_registry.register_instance(WidgetInstance {
                id: SceneId::new(),
                widget_type_name: "countdown".into(),
                tab_id: tab,
                instance_name: "clock".into(),
                geometry_override: Some(GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.0,
                    width_pct: 1.0,
                    height_pct: 0.62,
                }),
                contention_override: None,
                auto_clear_ms: None,
                current_params: bundle
                    .definition
                    .parameter_schema
                    .iter()
                    .map(|p| (p.name.clone(), p.default_value.clone()))
                    .collect(),
            });
            scene.widget_registry.register_definition(bundle.definition);
        }
        call(addr, "hud_publish", serde_json::json!({"surface":"widget:clock", "params":{"ends_in_ms":300000}, "delay_ms":10000, "ttl_ms":20000})).await;
        let pending_lease = runtime.events.scene().scheduled_publications[0]
            .publisher
            .lease_id
            .expect("MCP publication has an explicit lease");
        let shared = runtime.events.shared_state();
        let chrome = Arc::clone(&runtime.events.app.state.chrome_state);
        let portals = runtime
            .events
            .app
            .state
            .portal_projection_driver
            .portal_handle();
        assert!(
            crate::shell::safe_mode::enter_safe_mode(&shared, &chrome, Some(&portals))
                .await
                .entered
        );
        clock.advance(15_000);
        assert!(runtime.events.tick()); // Actual timed sweep while paused.
        {
            let scene = runtime.events.scene();
            assert_eq!(
                scene.scheduled_publications.len(),
                1,
                "accepted delayed MCP publication must remain pending during pause"
            );
            assert_eq!(
                scene.scheduled_publications[0].publisher.lease_id,
                Some(pending_lease)
            );
            assert_eq!(scene.next_timed_content_wall_us(), None);
            assert_eq!(scene.next_widget_deadline_wall_us(), None);
        }
        clock.advance(45_000);
        assert!(
            crate::shell::safe_mode::exit_safe_mode(&shared, &chrome, Some(&portals))
                .await
                .exited
        );
        assert!(
            runtime
                .events
                .scene()
                .widget_registry
                .active_for_widget("clock")
                .is_empty()
        );
        assert!(runtime.turn(true)); // First actual presentation is after resume.
        {
            let scene = runtime.events.scene();
            let record = &scene.widget_registry.active_for_widget("clock")[0];
            let tze_hud_scene::WidgetParameterValue::Deadline(deadline) =
                &record.params["ends_in_ms"]
            else {
                panic!("deadline")
            };
            assert_eq!(deadline.at_wall_us, Some(clock.now_us() + 300_000_000));
            assert_eq!(record.expires_at_wall_us, Some(clock.now_us() + 20_000_000));
            assert_eq!(record.lease_id, Some(pending_lease));
            assert_eq!(record.ttl_paused_us, 0, "never materialized during pause");
            assert!(scene.scheduled_publications.is_empty());
        }
        call(
            addr,
            "hud_hold",
            serde_json::json!({"surface":"widget:clock", "ttl_ms":0}),
        )
        .await;
        runtime.turn(true);
        assert!(!runtime.turn(true));
        let version = runtime.events.scene().version;
        let visible = |runtime: &CaptureRenderHarness| {
            runtime
                .compositor
                .widget_renderer()
                .unwrap()
                .texture_entry("clock")
                .unwrap()
                .last_rendered_params["ends_in_ms"]
                .clone()
        };
        assert_eq!(
            visible(&runtime),
            tze_hud_scene::WidgetParameterValue::String("5 min".into())
        );
        clock.advance(60_000);
        assert_eq!(runtime.events.scene().version, version);
        let capture = tokio::spawn({
            let endpoint = endpoint.clone();
            async move { endpoint.capture_frame(0).await }
        });
        queued.notified().await;
        assert!(
            runtime.turn(false),
            "due wall text must pass idle gate despite unchanged scene version"
        );
        let (_, permit) = capture.await.unwrap().unwrap();
        drop(permit);
        assert_eq!(
            visible(&runtime),
            tze_hud_scene::WidgetParameterValue::String("4 min".into())
        );
        assert!(
            runtime.turn(true),
            "capture/failed primary did not acknowledge screen repaint"
        );
        assert!(
            !runtime.turn(true),
            "countdown is not an inflight animation"
        );
        runtime.events.scene().pause_widget_ttls();
        clock.advance(120_000);
        assert!(!runtime.turn(true), "pause does not retry due countdowns");
        assert_eq!(
            runtime
                .compositor
                .next_widget_deadline_wall_us(&runtime.events.scene()),
            None
        );
        runtime.events.scene().resume_widget_ttls();
        assert!(runtime.turn(true));
        assert_eq!(
            visible(&runtime),
            tze_hud_scene::WidgetParameterValue::String("2 min".into())
        );
        clock.advance(1_000);
        assert!(runtime.turn(true));
        assert_eq!(
            visible(&runtime),
            tze_hud_scene::WidgetParameterValue::String("1:59".into())
        );
        assert!(!runtime.turn(true));
        call(
            addr,
            "hud_clear",
            serde_json::json!({"surface":"widget:clock"}),
        )
        .await;
        assert!(runtime.turn(true));
        assert_eq!(
            runtime
                .compositor
                .next_widget_deadline_wall_us(&runtime.events.scene()),
            None
        );
        assert!(!runtime.turn(true));
    }

    fn character(ch: &str, ts: u64) -> PendingKeyboardEvent {
        PendingKeyboardEvent::Character(RawCharacterEvent {
            character: ch.to_string(),
            timestamp_mono_us: MonoUs(ts),
        })
    }

    fn key_down(key_code: &str, key: &str, ts: u64) -> PendingKeyboardEvent {
        PendingKeyboardEvent::KeyDown(RawKeyDownEvent {
            key_code: key_code.to_string(),
            key: key.to_string(),
            modifiers: KeyboardModifiers::NONE,
            repeat: false,
            timestamp_mono_us: MonoUs(ts),
        })
    }

    fn key_up(key_code: &str, key: &str, ts: u64) -> PendingKeyboardEvent {
        PendingKeyboardEvent::KeyUp(RawKeyUpEvent {
            key_code: key_code.to_string(),
            key: key.to_string(),
            modifiers: KeyboardModifiers::NONE,
            timestamp_mono_us: MonoUs(ts),
        })
    }

    fn ctrl_key_down(key_code: &str, key: &str, shift: bool, ts: u64) -> PendingKeyboardEvent {
        PendingKeyboardEvent::KeyDown(RawKeyDownEvent {
            key_code: key_code.to_string(),
            key: key.to_string(),
            modifiers: KeyboardModifiers {
                ctrl: true,
                shift,
                ..KeyboardModifiers::NONE
            },
            repeat: false,
            timestamp_mono_us: MonoUs(ts),
        })
    }

    /// A harness showing one notification with Approve and Reject actions,
    /// ticked so its hit regions are live.
    fn approve_reject_notification_harness() -> HeadlessEventLoopHarness {
        approve_reject_notification_harness_on(Arc::new(tze_hud_scene::TestClock::new(1_000)))
    }

    fn approve_reject_notification_harness_on(clock: Arc<dyn Clock>) -> HeadlessEventLoopHarness {
        use tze_hud_scene::{
            ContentionPolicy, GeometryPolicy, LayerAttachment, NotificationAction,
            NotificationPayload, RenderingPolicy, ZoneContent, ZoneDefinition, ZoneMediaType,
        };

        let mut harness = HeadlessEventLoopHarness::new();
        {
            let shared = harness.app.state.shared_state.blocking_lock();
            let mut scene = shared.scene.blocking_lock();
            *scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock);
            scene.create_tab("Main", 0).unwrap();
            scene.register_zone(ZoneDefinition {
                id: SceneId::new(),
                name: "notification-area".to_string(),
                description: "notifications".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.6,
                    y_pct: 0.0,
                    width_pct: 0.35,
                    height_pct: 0.3,
                },
                accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::Stack { max_depth: 5 },
                max_publishers: 4,
                auto_clear_ms: None,
                layer_attachment: LayerAttachment::Chrome,
                ephemeral: false,
            });
            let action = |id: &str, label: &str| NotificationAction {
                copy: None,
                reply: None,
                label: label.to_string(),
                callback_id: Some(id.to_string()),
                url: None,
                role: None,
            };
            scene
                .publish_to_zone(
                    "notification-area",
                    ZoneContent::Notification(NotificationPayload {
                        progress: None,
                        text: "Click Approve".to_string(),
                        icon: String::new(),
                        urgency: 2,
                        ttl_ms: None,
                        title: "Action check".to_string(),
                        actions: vec![
                            action("approve", "Approve"),
                            action("reject", "Reject"),
                            action("wait", "Wait"),
                            action("details", "Details"),
                        ],
                    }),
                    "notif-agent",
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
        harness
            .app
            .state
            .global_tokens
            .insert("motion.exit.ms".into(), "0".into());
        while !harness.tick() {}
        harness
    }

    fn take_action_ids(harness: &HeadlessEventLoopHarness) -> Vec<String> {
        let shared = harness.app.state.shared_state.blocking_lock();
        let mut scene = shared.scene.blocking_lock();
        scene
            .take_pending_actions("notif-agent")
            .into_iter()
            .map(|action| match action.kind {
                tze_hud_scene::graph::PendingActionKind::Callback(id) => id,
                other => panic!("expected a publisher callback, received {other:?}"),
            })
            .collect()
    }

    fn dismiss_center(harness: &HeadlessEventLoopHarness) -> (f32, f32) {
        let shared = harness.app.state.shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        let dismiss = scene
            .overlay
            .zone_hit_regions
            .iter()
            .find(|region| matches!(region.kind, ZoneInteractionKind::Dismiss))
            .expect("the notification has a dismiss button");
        (
            dismiss.bounds.x + dismiss.bounds.width / 2.0,
            dismiss.bounds.y + dismiss.bounds.height / 2.0,
        )
    }

    /// A release that arrives after the press was already ended (the runtime
    /// saw the button physically up and synthesized the Up) must not repeat
    /// the click: one press on Approve queues exactly one `approve`.
    #[test]
    fn late_os_release_after_synthesized_up_queues_one_action() {
        let mut harness = approve_reject_notification_harness();
        let (x, y) = harness.notification_action_center("approve").unwrap();
        harness.app.state.cursor_x = x;
        harness.app.state.cursor_y = y;
        let window = winit::window::WindowId::dummy();

        harness.app.handle_left_button(true, window, None);
        // The button was seen physically up before winit's release arrived.
        harness.app.release_left_press();
        harness.app.handle_left_button(false, window, None);
        harness.drain();

        assert_eq!(take_action_ids(&harness), ["approve"]);
    }

    fn chrome_state(
        harness: &HeadlessEventLoopHarness,
        callback_id: &str,
    ) -> tze_hud_scene::types::ChromeLocalState {
        let shared = harness.app.state.shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        let id = scene
            .overlay
            .zone_hit_regions
            .iter()
            .find(|region| {
                scene
                    .notification_action(region)
                    .is_some_and(|a| a.callback_id.as_deref() == Some(callback_id))
            })
            .map(|region| region.interaction_id.clone())
            .expect("the action has a hit region");
        scene
            .overlay
            .chrome_states
            .get(&id)
            .copied()
            .unwrap_or_default()
    }

    fn geometry_epoch(harness: &HeadlessEventLoopHarness) -> u64 {
        let shared = harness.app.state.shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        scene.geometry_epoch
    }

    /// Hovering and pressing an action button set its local feedback state
    /// with no agent round trip, and each change re-arms the idle gate once
    /// (the geometry epoch) while an unchanged move does not.
    #[test]
    fn action_button_hover_and_press_are_local_and_frame_only_on_change() {
        // Retaining native history must also preserve the authorization of a
        // denied application edge queued behind a busy scene, and an exhausted
        // receipt queue cannot advertise a live hover before it can admit Down.
        {
            use crate::windowed::lifecycle::{ChromeAppEdge, ChromeCaptureTestScope};
            use winit::event::MouseButton;
            let mut failures = Vec::new();
            {
                let mut harness = approve_reject_notification_harness();
                let approve = harness.notification_action_center("approve").unwrap();
                let reject = harness.notification_action_center("reject").unwrap();
                let window = winit::window::WindowId::from(7);
                let native = ChromeCaptureTestScope::new(window);
                for down in [true, false] {
                    let frame = native.begin(down);
                    native.complete(frame);
                }
                drop(ChromeAppEdge::begin(winit::window::WindowId::from(8), true));
                assert!(harness.tick());
                let shared = harness.shared_state();
                let held = shared.blocking_lock();
                (harness.app.state.cursor_x, harness.app.state.cursor_y) = approve;
                for down in [true, false] {
                    harness
                        .app
                        .handle_mouse_button(MouseButton::Left, down, window, None);
                }
                (harness.app.state.cursor_x, harness.app.state.cursor_y) = reject;
                for down in [true, false] {
                    let frame = native.begin(down);
                    harness
                        .app
                        .handle_mouse_button(MouseButton::Left, down, window, None);
                    native.complete(frame);
                }
                assert_eq!(harness.app.state.pending_pointer_buttons.len(), 4);
                assert!(!harness.tick());
                assert_eq!(harness.app.state.pending_pointer_buttons.len(), 4);
                drop(held);
                let deadline = std::time::Instant::now() + SCENE_WAIT;
                // A settled tick has replayed every retained edge: input
                // still queued behind a transient lock miss counts as unsettled.
                while !harness.tick() {
                    let pending = harness.app.state.pending_pointer_buttons.len();
                    assert!(
                        std::time::Instant::now() < deadline,
                        "retained notification delivery stalled: {pending} edges"
                    );
                    std::thread::yield_now();
                }
                assert!(harness.app.state.pending_pointer_buttons.is_empty());
                let actions = take_action_ids(&harness);
                if actions != ["reject"] {
                    failures.push(format!(
                        "retained stale pair borrowed fresh authorization: {actions:?}"
                    ));
                }
            }
            {
                let mut harness = approve_reject_notification_harness();
                let approve = harness.notification_action_center("approve").unwrap();
                let window = winit::window::WindowId::from(7);
                let native = ChromeCaptureTestScope::new(window);
                for _ in 0..128 {
                    for down in [true, false] {
                        let frame = native.begin(down);
                        native.complete(frame);
                    }
                }
                drop(ChromeAppEdge::begin(winit::window::WindowId::from(8), true));
                assert!(harness.tick());
                assert!(native.healthy());
                harness.app.state.cursor_window = Some(window);
                (harness.app.state.cursor_x, harness.app.state.cursor_y) = approve;
                harness.app.enqueue_pointer_event(PointerEventKind::Move);
                let feedback = chrome_state(&harness, "approve");
                let frame = native.begin(true);
                assert!(frame.is_none(), "a full receipt queue cannot admit Down");
                harness
                    .app
                    .handle_mouse_button(MouseButton::Left, true, window, None);
                native.complete(frame);
                assert!(!chrome_state(&harness, "approve").pressed);
                assert!(take_action_ids(&harness).is_empty());
                if feedback != Default::default() {
                    failures.push(format!(
                        "full receipt queue advertised live feedback: {feedback:?}"
                    ));
                }
            }
            assert!(
                failures.is_empty(),
                "retained-history availability failures: {failures:?}"
            );
        }
        // Native callbacks can complete while winit buffers their application
        // events. A recovered press must never lend its receipt to that old pair.
        {
            use crate::windowed::lifecycle::{ChromeAppEdge, ChromeCaptureTestScope};
            use winit::event::MouseButton;
            for during_fault in [false, true] {
                let mut harness = approve_reject_notification_harness();
                let approve = harness.notification_action_center("approve").unwrap();
                let reject = harness.notification_action_center("reject").unwrap();
                let window = winit::window::WindowId::from(7);
                let native = ChromeCaptureTestScope::new(window);
                for down in [true, false] {
                    let frame = native.begin(down);
                    native.complete(frame);
                }
                drop(ChromeAppEdge::begin(winit::window::WindowId::from(8), true));
                assert!(!native.healthy());
                if during_fault {
                    for down in [true, false] {
                        let frame = native.begin(down);
                        assert!(frame.is_none(), "fault-era edges cannot authorize");
                        native.complete(frame);
                    }
                }
                assert!(harness.tick());
                assert!(native.healthy());
                if !during_fault {
                    for down in [true, false] {
                        let frame = native.begin(down);
                        native.complete(frame);
                    }
                }
                (harness.app.state.cursor_x, harness.app.state.cursor_y) = approve;
                if during_fault {
                    harness.app.state.cursor_window = Some(window);
                    harness.app.enqueue_pointer_event(PointerEventKind::Move);
                    assert_eq!(
                        chrome_state(&harness, "approve"),
                        Default::default(),
                        "pending quarantine stays inert after fault latch recovery"
                    );
                }
                for down in [true, false] {
                    harness
                        .app
                        .handle_mouse_button(MouseButton::Left, down, window, None);
                }
                assert!(
                    take_action_ids(&harness).is_empty(),
                    "buffered old application pair cannot consume fresh native authorization"
                );
                assert_eq!(harness.zone_publication_count("notification-area"), 1);
                (harness.app.state.cursor_x, harness.app.state.cursor_y) = reject;
                for down in [true, false] {
                    harness
                        .app
                        .handle_mouse_button(MouseButton::Left, down, window, None);
                }
                if during_fault {
                    assert!(
                        take_action_ids(&harness).is_empty(),
                        "fault-era application pair only drains cancelled debt"
                    );
                    assert_eq!(harness.zone_publication_count("notification-area"), 1);
                    for down in [true, false] {
                        let frame = native.begin(down);
                        harness
                            .app
                            .handle_mouse_button(MouseButton::Left, down, window, None);
                        native.complete(frame);
                    }
                }
                assert_eq!(
                    take_action_ids(&harness),
                    ["reject"],
                    "only matching fresh application coordinates may act"
                );
            }
        }
        // Winit can deliver a fresh native frame before flushing older buffered
        // application events. Ambiguity cancels all candidates, but each delivered
        // edge must still retire one debt unit so the next complete click works.
        {
            use crate::windowed::lifecycle::{ChromeAppEdge, ChromeCaptureTestScope};
            use winit::event::MouseButton;
            let mut harness = approve_reject_notification_harness();
            let approve = harness.notification_action_center("approve").unwrap();
            let reject = harness.notification_action_center("reject").unwrap();
            let window = winit::window::WindowId::from(7);
            let native = ChromeCaptureTestScope::new(window);
            for down in [true, false] {
                let frame = native.begin(down);
                native.complete(frame);
            }
            drop(ChromeAppEdge::begin(winit::window::WindowId::from(8), true));
            assert!(harness.tick());
            let frame = native.begin(true);
            (harness.app.state.cursor_x, harness.app.state.cursor_y) = reject;
            harness
                .app
                .handle_mouse_button(MouseButton::Left, true, window, None);
            (harness.app.state.cursor_x, harness.app.state.cursor_y) = approve;
            for down in [true, false] {
                harness
                    .app
                    .handle_mouse_button(MouseButton::Left, down, window, None);
            }
            native.complete(frame);
            let frame = native.begin(false);
            harness
                .app
                .handle_mouse_button(MouseButton::Left, false, window, None);
            native.complete(frame);
            assert!(
                take_action_ids(&harness).is_empty(),
                "reentrant ambiguous delivery never activates either candidate"
            );
            assert_eq!(harness.zone_publication_count("notification-area"), 1);
            assert!(harness.tick());
            assert!(native.healthy());
            (harness.app.state.cursor_x, harness.app.state.cursor_y) = reject;
            for down in [true, false] {
                let frame = native.begin(down);
                harness
                    .app
                    .handle_mouse_button(MouseButton::Left, down, window, None);
                native.complete(frame);
            }
            assert_eq!(
                take_action_ids(&harness),
                ["reject"],
                "cancelled ambiguous edges leave no phantom debt"
            );
        }
        // A capture fault and safe mode both make notification buttons inert.
        // Inject only native receipt delivery; routing, feedback, cancellation
        // and between-turn recovery below use the production implementations.
        {
            use crate::windowed::lifecycle::{ChromeAppEdge, ChromeCaptureTestScope};
            use winit::event::MouseButton;

            let mut missing = Vec::new();
            for fault in [false, true] {
                let mut harness = approve_reject_notification_harness();
                let approve = harness.notification_action_center("approve").unwrap();
                (harness.app.state.cursor_x, harness.app.state.cursor_y) = approve;
                let window = winit::window::WindowId::from(7);
                let native = ChromeCaptureTestScope::new(window);
                harness.app.state.press_window = Some(window);
                harness.app.state.cursor_window = Some(window);
                let in_flight = if fault {
                    let frame = native.begin(true);
                    harness
                        .app
                        .handle_mouse_button(MouseButton::Left, true, window, None);
                    native.complete(frame);
                    assert!(chrome_state(&harness, "approve").pressed);
                    assert!(harness.app.state.zone_press.is_some());
                    let frame = native.begin(false);
                    // An application edge without matching native history
                    // faults while another native delivery is still active.
                    drop(ChromeAppEdge::begin(winit::window::WindowId::from(8), true));
                    assert!(!native.healthy());
                    assert!(harness.tick());
                    assert!(!native.healthy(), "in-flight history cannot recover");
                    assert!(
                        harness.app.state.zone_press.is_none(),
                        "fault disarms the accepted press"
                    );
                    frame
                } else {
                    harness
                        .app
                        .state
                        .safe_mode_atomic
                        .store(true, std::sync::atomic::Ordering::Release);
                    None
                };
                harness.app.enqueue_pointer_event(PointerEventKind::Move);
                if chrome_state(&harness, "approve").hovered {
                    missing.push((fault, "inert hover"));
                }
                let frame = native.begin(true);
                harness
                    .app
                    .handle_mouse_button(MouseButton::Left, true, window, None);
                native.complete(frame);
                if chrome_state(&harness, "approve").pressed {
                    missing.push((fault, "inert press"));
                }
                assert!(take_action_ids(&harness).is_empty());

                if fault {
                    native.complete(in_flight);
                    let edge = ChromeAppEdge::begin(window, false);
                    assert!(harness.tick());
                    assert!(
                        !native.healthy(),
                        "an application edge cannot recover itself"
                    );
                    drop(edge);
                    // Fault cancellation must reach the scene before recovery.
                    harness.app.cancel_chrome_feedback();
                    let shared = harness.shared_state();
                    let held = shared.blocking_lock();
                    for down in [true, false] {
                        harness
                            .app
                            .handle_mouse_button(MouseButton::Left, down, window, None);
                    }
                    assert!(!harness.app.state.pending_pointer_buttons.is_empty());
                    assert!(!harness.tick());
                    assert!(!native.healthy(), "busy feedback cleanup stays fail-closed");
                    drop(held);
                    let deadline = std::time::Instant::now() + SCENE_WAIT;
                    while !harness.tick() {
                        assert!(std::time::Instant::now() < deadline);
                        std::thread::yield_now();
                    }
                    assert!(harness.app.state.pending_pointer_buttons.is_empty());
                    assert!(
                        take_action_ids(&harness).is_empty(),
                        "retained fault-era edges cannot activate"
                    );
                    if !native.healthy() {
                        missing.push((fault, "between-turn recovery"));
                    }
                    assert_eq!(chrome_state(&harness, "approve"), Default::default());
                } else {
                    harness
                        .app
                        .state
                        .safe_mode_atomic
                        .store(false, std::sync::atomic::Ordering::Release);
                }
                // The cancelled/disabled press's Up cannot activate, even
                // after recovery. Only a complete fresh press may act.
                let frame = native.begin(false);
                harness
                    .app
                    .handle_mouse_button(MouseButton::Left, false, window, None);
                native.complete(frame);
                assert!(take_action_ids(&harness).is_empty(), "late Up stays inert");
                assert_eq!(harness.zone_publication_count("notification-area"), 1);
                harness.app.state.cursor_window = Some(winit::window::WindowId::from(8));
                harness.app.enqueue_pointer_event(PointerEventKind::Move);
                assert_eq!(
                    chrome_state(&harness, "approve"),
                    Default::default(),
                    "an unobserved window stays inert"
                );
                harness.app.state.cursor_window = Some(window);
                for down in [true, false] {
                    let frame = native.begin(down);
                    harness
                        .app
                        .handle_mouse_button(MouseButton::Left, down, window, None);
                    native.complete(frame);
                }
                harness.drain();
                if take_action_ids(&harness) != ["approve"] {
                    missing.push((fault, "fresh click after recovery"));
                }
            }
            assert!(
                missing.is_empty(),
                "notification availability failures: {missing:?}"
            );
        }
        let mut harness = approve_reject_notification_harness();
        let approve = harness.notification_action_center("approve").unwrap();
        let reject = harness.notification_action_center("reject").unwrap();

        let focus_id = harness
            .scene()
            .overlay
            .zone_hit_regions
            .iter()
            .find(|r| matches!(&r.kind, ZoneInteractionKind::Action { index: 0 }))
            .unwrap()
            .interaction_id
            .clone();
        let before_focus = geometry_epoch(&harness);
        harness.scene().focus_notification(Some(focus_id.clone()));
        assert!(geometry_epoch(&harness) > before_focus);
        let focused_epoch = geometry_epoch(&harness);
        harness.scene().focus_notification(Some(focus_id));
        assert_eq!(
            geometry_epoch(&harness),
            focused_epoch,
            "unchanged local focus stays idle"
        );
        harness.scene().focus_notification(None);
        assert!(
            take_action_ids(&harness).is_empty(),
            "focus requires no model round trip"
        );
        let epoch = geometry_epoch(&harness);
        harness.app.state.cursor_x = approve.0;
        harness.app.state.cursor_y = approve.1;
        harness.app.enqueue_pointer_event(PointerEventKind::Move);
        let hovered = chrome_state(&harness, "approve");
        assert!(hovered.hovered && !hovered.pressed);
        assert!(
            geometry_epoch(&harness) > epoch,
            "hover-on requests a frame"
        );

        let epoch = geometry_epoch(&harness);
        harness.app.state.cursor_x = approve.0 + 1.0;
        harness.app.enqueue_pointer_event(PointerEventKind::Move);
        assert_eq!(
            geometry_epoch(&harness),
            epoch,
            "moving within the hovered button requests no frame"
        );

        harness.app.enqueue_pointer_event(PointerEventKind::Down);
        assert!(chrome_state(&harness, "approve").pressed);
        assert!(geometry_epoch(&harness) > epoch, "press requests a frame");

        // Sliding off onto Reject moves the hover; Approve stays pressed
        // until the release, which then activates nothing.
        harness.app.state.cursor_x = reject.0;
        harness.app.state.cursor_y = reject.1;
        harness.app.enqueue_pointer_event(PointerEventKind::Move);
        let approve_state = chrome_state(&harness, "approve");
        assert!(approve_state.pressed && !approve_state.hovered);
        assert!(chrome_state(&harness, "reject").hovered);
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        assert_eq!(
            chrome_state(&harness, "approve"),
            Default::default(),
            "release clears the press"
        );
        harness.drain();
        assert!(take_action_ids(&harness).is_empty());
        assert_eq!(harness.zone_publication_count("notification-area"), 1);

        // The same production terminal operation is called by accepted
        // CursorLeft, focus loss, and observed loss of an armed native capture.
        // It retains intent while busy; this headless parent does not claim a
        // native Windows event/capture or physical-display observation.
        harness.app.state.cursor_x = approve.0;
        harness.app.state.cursor_y = approve.1;
        harness.app.enqueue_pointer_event(PointerEventKind::Move);
        harness.app.enqueue_pointer_event(PointerEventKind::Down);
        let epoch = geometry_epoch(&harness);
        let shared = harness.shared_state();
        let held = shared.blocking_lock();
        harness.app.cancel_chrome_feedback();
        assert!(harness.app.state.zone_press.is_none());
        assert!(harness.app.settle_scene_work().is_deferred());
        assert!(
            harness
                .app
                .state
                .input_processor
                .chrome_feedback_cancel_pending
        );
        drop(held);
        assert!(harness.tick());
        assert!(
            !harness
                .app
                .state
                .input_processor
                .chrome_feedback_cancel_pending
        );
        assert_eq!(chrome_state(&harness, "approve"), Default::default());
        assert!(
            geometry_epoch(&harness) > epoch,
            "terminal cleanup requests a frame"
        );
        let epoch = geometry_epoch(&harness);
        harness.app.cancel_chrome_feedback();
        assert!(harness.tick());
        assert_eq!(
            geometry_epoch(&harness),
            epoch,
            "repeated cleanup stays idle"
        );
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        assert!(
            take_action_ids(&harness).is_empty(),
            "cancel is not an activation"
        );
        harness.app.enqueue_pointer_event(PointerEventKind::Move);
        assert!(
            chrome_state(&harness, "approve").hovered,
            "return can hover the same control"
        );

        // Existing lifetime owners remove hit regions and their feedback. The
        // replacement changes publication identity even when its labels match.
        for terminal in ["clear", "expiry", "replacement", "dismiss"] {
            let mut harness = approve_reject_notification_harness();
            let original = harness.notification_action_center("approve").unwrap();
            let placement = tze_hud_scene::GeometryPolicy::Relative {
                x_pct: 0.5,
                y_pct: 0.2,
                width_pct: 0.35,
                height_pct: 0.3,
            };
            {
                let mut scene = harness.scene();
                let id = scene.zone_registry.zones["notification-area"].id;
                let version = scene.version;
                let epoch = scene.geometry_epoch;
                assert!(scene.set_zone_placement(id, placement));
                assert_eq!(scene.geometry_epoch, epoch + 1);
                assert!(!scene.set_zone_placement(id, placement));
                assert_eq!(scene.geometry_epoch, epoch + 1);
                assert_eq!(scene.version, version);
            }
            assert!(harness.tick());
            let approve = harness.notification_action_center("approve").unwrap();
            assert!((approve.0 - original.0 + 192.0).abs() < 0.1);
            assert!((approve.1 - original.1 - 216.0).abs() < 0.1);
            assert_eq!(
                harness.scene().overlay.zone_hit_regions.len(),
                5,
                "all four actions plus dismiss move together"
            );
            harness.app.state.cursor_x = approve.0;
            harness.app.state.cursor_y = approve.1;
            harness.app.enqueue_pointer_event(PointerEventKind::Move);
            harness.app.enqueue_pointer_event(PointerEventKind::Down);
            let old_id = harness.app.state.zone_press.clone().unwrap();
            {
                let shared = harness.app.state.shared_state.blocking_lock();
                let mut scene = shared.scene.blocking_lock();
                let old = scene.zone_registry.active_for_zone("notification-area")[0].clone();
                match terminal {
                    "clear" => scene.clear_zone("notification-area").unwrap(),
                    "expiry" => {
                        scene
                            .zone_registry
                            .active_publishes
                            .get_mut("notification-area")
                            .unwrap()[0]
                            .expires_at_wall_us = Some(0);
                    }
                    "replacement" => {
                        scene.clear_zone("notification-area").unwrap();
                        scene
                            .publish_to_zone(
                                "notification-area",
                                old.content,
                                "notif-agent",
                                None,
                                None,
                                None,
                            )
                            .unwrap();
                    }
                    "dismiss" => assert!(scene.dismiss_notification(
                        "notification-area",
                        old.publication_id,
                        "notif-agent"
                    )),
                    _ => unreachable!(),
                }
            }
            assert!(harness.tick());
            {
                let shared = harness.app.state.shared_state.blocking_lock();
                let scene = shared.scene.blocking_lock();
                assert!(
                    !scene.overlay.chrome_states.contains_key(&old_id),
                    "{terminal}"
                );
            }
            harness.app.enqueue_pointer_event(PointerEventKind::Up);
            assert!(
                take_action_ids(&harness).is_empty(),
                "{terminal} cannot activate a stale press"
            );
            let epoch = geometry_epoch(&harness);
            assert!(harness.tick());
            assert_eq!(geometry_epoch(&harness), epoch, "{terminal} settles idle");
            let scene = harness.scene();
            assert_eq!(
                scene.effective_zone_geometry(&scene.zone_registry.zones["notification-area"]),
                placement
            );
        }

        // Composer hit regions use the same runtime pointer entry point, but
        // their local feedback must repaint without waiting for a caret blink.
        let mut composer = HeadlessEventLoopHarness::new();
        composer.focus_composer();
        composer.app.state.cursor_x = 10.0;
        composer.app.state.cursor_y = 10.0;
        composer.app.enqueue_pointer_event(PointerEventKind::Up);
        let content_version = composer.scene().version;
        for (kind, y, changed) in [
            (PointerEventKind::Move, 120.0, true),
            (PointerEventKind::Move, 121.0, false),
            (PointerEventKind::Move, 10.0, true),
            (PointerEventKind::Move, 11.0, false),
            (PointerEventKind::Down, 11.0, true),
            (PointerEventKind::Up, 11.0, true),
            (PointerEventKind::Move, 120.0, true),
        ] {
            let epoch = geometry_epoch(&composer);
            composer.app.state.cursor_y = y;
            composer.app.enqueue_pointer_event(kind.clone());
            assert_eq!(
                geometry_epoch(&composer) > epoch,
                changed,
                "composer {kind:?} at y={y} repaints only changed feedback"
            );
            assert_eq!(composer.scene().version, content_version);
        }

        // Model only the emitted handle/window identity at the headless boundary.
        // Admission, activation, native ledger loss, focus cancellation, retained
        // Up and scene settling below are the production routes, not reset helpers.
        // The render parent separately proves actual compositor handle production.
        // Tile cancellation semantics remain unchanged at both real routing seams.
        {
            use crate::windowed::lifecycle::{ChromeAppEdge, ChromeCaptureTestScope};
            let mut harness = approve_reject_notification_harness();
            let (scene, tile_id, ..) =
                super::super::test_support::scene_with_drag_handle_tile(400.0, 300.0, 600.0, 200.0);
            let handle = scene.overlay.drag_handle_hit_regions[0].bounds;
            *harness.scene() = scene;
            (harness.app.state.cursor_x, harness.app.state.cursor_y) =
                (handle.x + 1.0, handle.y + 1.0);
            harness.app.enqueue_pointer_event(PointerEventKind::Down);
            harness.app.state.cursor_y += 1.0;
            harness.app.enqueue_pointer_event(PointerEventKind::Move);
            let press_start = harness
                .app
                .state
                .primary_drag_gesture
                .as_ref()
                .unwrap()
                .press_start;
            harness.app.handle_window_focus(None, false);
            let window = winit::window::WindowId::dummy();
            let native = ChromeCaptureTestScope::new(window);
            let frame = native.begin(true);
            let edge = ChromeAppEdge::begin(window, true);
            drop(edge);
            native.complete(frame);
            harness.app.state.press_window = Some(window);
            harness
                .app
                .state
                .primary_drag_gesture
                .as_mut()
                .unwrap()
                .window_id = Some(window);
            native.lose();
            harness.app.drain_chrome_capture_cancellation();
            assert_eq!(
                harness
                    .app
                    .state
                    .primary_drag_gesture
                    .as_ref()
                    .unwrap()
                    .press_start,
                press_start
            );
            assert!(
                harness
                    .app
                    .state
                    .input_processor
                    .drag_states
                    .contains_key(&0)
            );
            harness
                .app
                .state
                .primary_drag_gesture
                .as_mut()
                .unwrap()
                .window_id = None;
            harness.app.state.press_window = None;
            drop(native);
            harness.app.state.cursor_y += 43.2;
            harness.app.enqueue_pointer_event(PointerEventKind::Move);
            harness.app.enqueue_pointer_event(PointerEventKind::Up);
            assert!(harness.scene().tiles[&tile_id].bounds.y > 300.0);
            assert!(!harness.scene().is_drag_active(tile_id));
            assert!(harness.app.state.primary_drag_gesture.is_none());
        }

        let mut cancellations_not_disarmed = Vec::new();
        for native_loss in [false, true] {
            for held_terminal in [false, true] {
                use crate::windowed::lifecycle::{ChromeAppEdge, ChromeCaptureTestScope};
                let mut harness = approve_reject_notification_harness();
                let (fixture, tile_id, ..) =
                    super::super::test_support::scene_with_drag_handle_tile(
                        400.0, 300.0, 600.0, 200.0,
                    );
                let id = {
                    let mut scene = harness.scene();
                    let id = scene.zone_registry.zones["notification-area"].id;
                    let mut handle = fixture.overlay.drag_handle_hit_regions[0].clone();
                    handle.element_id = id;
                    handle.element_kind = tze_hud_scene::DragHandleElementKind::Zone;
                    (harness.app.state.cursor_x, harness.app.state.cursor_y) =
                        (handle.bounds.x + 1.0, handle.bounds.y + 1.0);
                    scene.overlay.drag_handle_hit_regions.push(handle);
                    scene.tiles.extend(fixture.tiles.clone());
                    id
                };

                harness.app.enqueue_pointer_event(PointerEventKind::Down);
                harness.app.state.cursor_y += 1.0;
                harness.app.enqueue_pointer_event(PointerEventKind::Move);
                harness.app.state.cursor_y += 43.2;
                harness.app.enqueue_pointer_event(PointerEventKind::Move);
                assert!(harness.scene().is_drag_active(id));
                let placement = harness.scene().overlay.zone_placements[&id];
                let mut other_device = harness.app.state.input_processor.drag_states[&0].clone();
                other_device.element_id = tile_id;
                other_device.element_kind = tze_hud_scene::DragHandleElementKind::Tile;
                harness
                    .app
                    .state
                    .input_processor
                    .drag_states
                    .insert(1, other_device);
                harness.scene().set_drag_active(tile_id);
                let window = winit::window::WindowId::dummy();
                let native = native_loss.then(|| {
                    let native = ChromeCaptureTestScope::new(window);
                    let frame = native.begin(true);
                    let edge = ChromeAppEdge::begin(window, true);
                    drop(edge);
                    native.complete(frame);
                    harness.app.state.press_window = Some(window);
                    harness
                        .app
                        .state
                        .primary_drag_gesture
                        .as_mut()
                        .unwrap()
                        .window_id = Some(window);
                    native
                });
                let shared = harness.shared_state();
                let held = held_terminal.then(|| shared.blocking_lock());
                if held_terminal {
                    harness.app.state.cursor_y += 43.2;
                    harness.app.enqueue_pointer_event(PointerEventKind::Up);
                    assert!(
                        harness
                            .app
                            .state
                            .primary_drag_gesture
                            .as_ref()
                            .unwrap()
                            .terminal
                            .is_some()
                    );
                }
                if let Some(native) = &native {
                    native.lose();
                    harness.app.state.press_window = Some(winit::window::WindowId::from(77));
                    harness.app.drain_chrome_capture_cancellation();
                    assert!(
                        harness.app.state.primary_drag_gesture.is_some(),
                        "another window cannot consume this capture cancellation"
                    );
                    harness.app.state.press_window = Some(window);
                    harness.app.drain_chrome_capture_cancellation();
                } else {
                    harness.app.handle_window_focus(None, false);
                }
                if harness.app.state.primary_drag_gesture.is_some()
                    || harness
                        .app
                        .state
                        .input_processor
                        .drag_states
                        .contains_key(&0)
                {
                    cancellations_not_disarmed.push((native_loss, held_terminal));
                    continue;
                }
                if held_terminal {
                    assert!(harness.app.settle_scene_work().is_deferred());
                }
                drop(held);
                // Force the later prune borrow to miss after the terminal chrome
                // cleanup succeeds. The unfinished reconciliation must retain
                // the existing scene-availability continuation.
                {
                    let state = shared.blocking_lock();
                    let mut scene = state.scene.blocking_lock();
                    harness
                        .app
                        .state
                        .input_processor
                        .clear_chrome_feedback(&mut scene);
                }
                assert!(
                    !harness
                        .app
                        .state
                        .input_processor
                        .chrome_feedback_cancel_pending
                );
                let held = shared.blocking_lock();
                assert!(
                    harness.app.prune_portal_resize_states().is_deferred(),
                    "a missed prune borrow must retain the cancellation continuation"
                );
                if held_terminal {
                    harness.schedule_shared_scene_availability_wake();
                }
                drop(held);
                let deadline = std::time::Instant::now() + SCENE_WAIT;
                while !harness.tick() {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::yield_now();
                }
                assert!(!harness.scene().is_drag_active(id));
                assert!(
                    harness.scene().is_drag_active(tile_id),
                    "other device keeps Tile feedback"
                );
                assert!(
                    harness
                        .app
                        .state
                        .input_processor
                        .drag_states
                        .contains_key(&1)
                );
                let epoch = geometry_epoch(&harness);
                assert!(harness.tick());
                assert_eq!(geometry_epoch(&harness), epoch, "cancel settles once");
                harness.app.state.cursor_y += 43.2;
                harness.app.enqueue_pointer_event(PointerEventKind::Move);
                harness.app.enqueue_pointer_event(PointerEventKind::Up);
                assert_eq!(
                    harness.scene().overlay.zone_placements[&id],
                    placement,
                    "late ordinary or retained terminal cannot move cancelled placement"
                );

                assert!(shared.blocking_lock().element_store.entries.is_empty());
                assert!(take_action_ids(&harness).is_empty());
                assert_eq!(harness.zone_publication_count("notification-area"), 1);
                drop(native);
                harness.app.state.press_window = None;
                // A deliberate new Down still owns a fresh gesture after cancellation.
                harness.app.state.cursor_y = 291.0;
                harness.app.enqueue_pointer_event(PointerEventKind::Down);
                harness.app.state.cursor_y += 1.0;
                harness.app.enqueue_pointer_event(PointerEventKind::Move);
                harness.app.state.cursor_y += 43.2;
                harness.app.enqueue_pointer_event(PointerEventKind::Move);
                assert!(harness.scene().is_drag_active(id));
                let stale = harness.app.state.primary_drag_gesture.clone();
                harness.app.state.cursor_y = 291.0;
                harness.app.enqueue_pointer_event(PointerEventKind::Down);
                harness.app.state.cursor_y += 1.0;
                harness.app.enqueue_pointer_event(PointerEventKind::Move);
                let newer_press = harness.app.state.input_processor.drag_states[&0].press_start;
                // Retained identity can never remove a newer same-device Down.
                harness.app.state.primary_drag_gesture = stale;
                harness.app.handle_window_focus(None, false);
                assert_eq!(
                    harness.app.state.input_processor.drag_states[&0].press_start,
                    newer_press
                );
                assert!(harness.tick());
                assert!(harness.scene().is_drag_active(id));
                harness.app.enqueue_pointer_event(PointerEventKind::Up);
                assert!(!harness.scene().is_drag_active(id));
            }
        }

        assert!(
            cancellations_not_disarmed.is_empty(),
            "accepted cancellation must immediately disarm Zone/retained Up; failed (native, held): {cancellations_not_disarmed:?}"
        );

        // A known terminal cancellation must disarm before the physical-Up
        // fallback, even if the ordinary late Up follows in the same turn.
        // This exercises ordering after an observation, not native delivery.
        let mut harness = approve_reject_notification_harness();
        let (x, y) = harness.notification_action_center("approve").unwrap();
        harness.app.state.cursor_x = x;
        harness.app.state.cursor_y = y;
        let window = winit::window::WindowId::dummy();
        harness.app.handle_left_button(true, window, None);
        harness.app.cancel_chrome_feedback();
        harness.app.release_left_press();
        harness.app.handle_left_button(false, window, None);
        assert!(
            take_action_ids(&harness).is_empty(),
            "capture cancellation precedes fallback Up"
        );

        // Same production receipt domain, deterministic data only: native
        // callback ordering and physical Windows delivery remain separate.
        use crate::windowed::lifecycle::ChromeCaptureLedger;
        let mut receipts = ChromeCaptureLedger::default();
        let install = receipts.install(window).unwrap();
        let down = receipts.begin(install, true).unwrap();
        assert!(receipts.consume(window, true), "direct active Down");
        receipts.complete(down);
        let up = receipts.begin(install, false).unwrap();
        assert!(receipts.consume(window, false), "normal Up activates once");
        receipts.complete(up);
        let late = receipts.begin(install, false).unwrap();
        assert!(
            !receipts.consume(window, false),
            "late Up is not another action"
        );
        receipts.complete(late);

        let mut receipts = ChromeCaptureLedger::default();
        let install = receipts.install(window).unwrap();
        let down = receipts.begin(install, true).unwrap();
        assert!(receipts.consume(window, true));
        let up = receipts.begin(install, false).unwrap();
        receipts.complete(up);
        assert!(
            receipts.consume(window, false),
            "consumed active frame is not ambiguous"
        );
        receipts.complete(down);

        let mut receipts = ChromeCaptureLedger::default();
        let install = receipts.install(window).unwrap();
        let down = receipts.begin(install, true).unwrap();
        assert!(receipts.consume(window, true));
        receipts.complete(down);
        assert!(receipts.lose(install));
        assert!(
            receipts.take_cancel(window),
            "known direct loss disarms current press"
        );
        let up = receipts.begin(install, false).unwrap();
        assert!(
            !receipts.consume(window, false),
            "ordinary Up cannot revive lost capture"
        );
        receipts.complete(up);

        let mut receipts = ChromeCaptureLedger::default();
        let install = receipts.install(window).unwrap();
        let down = receipts.begin(install, true).unwrap();
        receipts.complete(down); // borrowed app: retain history before delivery
        assert!(receipts.lose(install));
        let up = receipts.begin(install, false).unwrap();
        receipts.complete(up);
        let next = receipts.begin(install, true).unwrap();
        receipts.complete(next);
        assert!(
            !receipts.consume(window, true),
            "buffered loss belongs to old Down"
        );
        assert!(!receipts.consume(window, false));
        assert!(
            receipts.consume(window, true),
            "later press keeps its own token"
        );
        assert!(
            !receipts.take_cancel(window),
            "old cancellation cannot erase new press"
        );
        receipts.finish_synthetic(window);
        let late = receipts.begin(install, false).unwrap();
        assert!(
            !receipts.consume(window, false),
            "synthetic release finishes same token"
        );
        receipts.complete(late);

        let mut receipts = ChromeCaptureLedger::default();
        let old = receipts.install(window).unwrap();
        let down = receipts.begin(old, true).unwrap();
        receipts.complete(down);
        receipts.retire(old);
        let replacement = receipts.install(window).unwrap();
        assert!(
            !receipts.consume(window, true),
            "retired HWND installation stays inert"
        );
        let down = receipts.begin(replacement, true).unwrap();
        assert!(receipts.consume(window, true));
        receipts.complete(down);
        assert!(
            !receipts.take_cancel(window),
            "old installation cannot cancel replacement"
        );

        let mut receipts = ChromeCaptureLedger::default();
        let install = receipts.install(window).unwrap();
        let buffered = receipts.begin(install, true).unwrap();
        receipts.complete(buffered);
        let active = receipts.begin(install, false).unwrap();
        assert!(
            !receipts.consume(window, true),
            "completed plus active is ambiguous"
        );
        receipts.complete(active);
        assert!(
            !receipts.permit(window),
            "ambiguity disables until explicit recovery"
        );
        assert!(receipts.install(window).is_none(), "no automatic rearm");
        assert!(
            receipts.recover(),
            "between-turn recovery discards ambiguous history"
        );
        assert!(
            !receipts.permit(window),
            "recovery cannot authorize an old press"
        );
        assert!(
            !receipts.consume(window, false),
            "buffered cancelled Up keeps its own receipt"
        );
        let late = receipts.begin(install, false).unwrap();
        assert!(late > active, "frame identities never rewind");
        assert!(
            !receipts.consume(window, false),
            "held old Up stays cancelled"
        );
        receipts.complete(late);
        receipts.retire(install);
        let replacement = receipts.install(window).unwrap();
        assert!(
            replacement > install,
            "reinstallation identities never rewind"
        );
        let late = receipts.begin(replacement, false).unwrap();
        assert!(
            !receipts.consume(window, false),
            "reinstallation cannot arm an Up"
        );
        receipts.complete(late);
        for down in [true, false] {
            let edge = receipts.begin(replacement, down).unwrap();
            assert!(
                receipts.consume(window, down),
                "fresh replacement click works"
            );
            receipts.complete(edge);
        }
        assert!(!receipts.recover(), "healthy recovery is a no-op");

        let mut receipts = ChromeCaptureLedger::default();
        let old = receipts.install(window).unwrap();
        receipts.fault();
        assert!(receipts.begin(old, true).is_none());
        assert!(receipts.recover());
        receipts.retire(old);
        let replacement = receipts.install(window).unwrap();
        assert!(replacement > old);
        assert!(
            receipts.begin(replacement, true).is_none(),
            "retired delivery debt cannot be bypassed by reinstallation"
        );
        assert!(!receipts.consume(window, true));
        assert!(!receipts.consume(window, true));
        for down in [true, false] {
            let edge = receipts.begin(replacement, down).unwrap();
            assert!(
                receipts.consume(window, down),
                "fresh replacement click follows the retired debt drain"
            );
            receipts.complete(edge);
        }

        let mut receipts = ChromeCaptureLedger::default();
        let install = receipts.install(window).unwrap();
        for _ in 0..256 {
            let edge = receipts.begin(install, true).unwrap();
            receipts.complete(edge);
        }
        assert!(
            receipts.begin(install, true).is_none(),
            "bounded receipt admission"
        );
        assert!(!receipts.consume(window, true));
        assert!(
            receipts.recover(),
            "bounded receipt failure can resynchronize"
        );
        assert!(
            receipts.begin(install, true).is_none(),
            "overflow delivery debt cannot admit a new press"
        );
        for _ in 0..257 {
            assert!(
                !receipts.consume(window, true),
                "known and overflow application edges drain inert"
            );
        }
        let down = receipts.begin(install, true).unwrap();
        assert!(receipts.consume(window, true));
        receipts.complete(down);
        let mut receipts = ChromeCaptureLedger::default();
        for _ in 0..64 {
            assert!(receipts.install(window).is_some());
        }
        assert!(
            receipts.install(window).is_none(),
            "bounded installation admission"
        );
        assert!(receipts.recover());
        assert!(
            receipts.install(window).is_none(),
            "recovery cannot bypass the installation bound"
        );
    }

    /// Sends a held link worker its result: `Ok` opens, `Err` fails.
    type LinkVerdict = std::sync::mpsc::Sender<Result<(), String>>;

    /// A card harness whose first two actions are links, with a link worker
    /// that waits for the test's verdict.
    fn link_card_harness(
        clock: Arc<tze_hud_scene::TestClock>,
    ) -> (HeadlessEventLoopHarness, LinkVerdict, [(f32, f32); 2]) {
        use tze_hud_scene::{NotificationAction, ZoneContent};
        let mut harness = approve_reject_notification_harness_on(clock);
        let urls = ["https://example.com/one", "https://example.com/two"];
        {
            let mut scene = harness.scene();
            let ZoneContent::Notification(payload) = &mut scene
                .zone_registry
                .active_publishes
                .get_mut("notification-area")
                .unwrap()[0]
                .content
            else {
                unreachable!()
            };
            for (i, url) in urls.iter().enumerate() {
                payload.actions[i] = NotificationAction {
                    copy: None,
                    reply: None,
                    label: format!("Link {i}"),
                    callback_id: None,
                    url: Some((*url).into()),
                    role: None,
                };
            }
        }
        assert!(harness.tick());
        let (verdict, verdicts) = std::sync::mpsc::channel::<Result<(), String>>();
        let verdicts = std::sync::Mutex::new(verdicts);
        harness.app.state.notification_link_opener.worker = Some(Arc::new(move |_url| {
            verdicts
                .lock()
                .unwrap()
                .recv()
                .unwrap_or(Err("test ended".into()))
        }));
        let points = urls.map(|url| harness.notification_link_center(url).unwrap());
        (harness, verdict, points)
    }

    fn click_at(harness: &mut HeadlessEventLoopHarness, point: (f32, f32)) {
        use super::super::lifecycle::ChromeCaptureTestScope;
        let window = winit::window::WindowId::dummy();
        let native = ChromeCaptureTestScope::new(window);
        (harness.app.state.cursor_x, harness.app.state.cursor_y) = point;
        for down in [true, false] {
            let receipt = native.begin(down);
            harness
                .app
                .handle_mouse_button(winit::event::MouseButton::Left, down, window, None);
            native.complete(receipt);
        }
        harness.drain();
    }

    #[test]
    fn notification_reveal_arms_only_for_cut_text_and_esc_suppresses_the_focused_field() {
        use std::time::{Duration, Instant};
        use tze_hud_scene::graph::overlay::{
            NotificationRevealField, NotificationRevealRegion, NotificationRevealTarget,
        };
        let mut harness = approve_reject_notification_harness();
        let start = Instant::now();
        assert!(!harness.app.settle_notification_reveal(start));
        assert_eq!(harness.app.state.notification_reveal.next_deadline(), None);
        let (id, bounds) = {
            let scene = harness.scene();
            let card = &scene.overlay.notification_geometry[0];
            (card.publication_id, card.bounds)
        };
        let target = NotificationRevealTarget {
            publication_id: id,
            field: NotificationRevealField::Body,
        };
        let focus_id = format!("notification-reveal:{id}:body");
        let source = Rect::new(bounds.x, bounds.y, bounds.width * 0.5, bounds.height * 0.25);
        let other = NotificationRevealTarget {
            field: NotificationRevealField::Title,
            ..target
        };
        let other_source = Rect::new(
            source.x + source.width,
            source.y,
            source.width,
            source.height,
        );
        let snapshot = {
            let mut scene = harness.scene();
            scene.overlay.notification_reveal_regions.extend([
                NotificationRevealRegion {
                    target,
                    bounds: source,
                    focus_id: focus_id.clone(),
                },
                NotificationRevealRegion {
                    target: other,
                    bounds: other_source,
                    focus_id: format!("notification-reveal:{id}:title"),
                },
            ]);
            crate::pipeline::HitTestSnapshot::from_scene(&scene)
        };
        harness
            .app
            .state
            .pipeline
            .hit_test_snapshot
            .store(Arc::new(snapshot));
        harness.app.state.cursor_x = source.x + source.width * 0.5;
        harness.app.state.cursor_y = source.y + source.height * 0.5;
        assert!(!harness.app.settle_notification_reveal(start));
        let deadline = start + Duration::from_millis(180);
        assert_eq!(
            harness.app.state.notification_reveal.next_deadline(),
            Some(deadline)
        );
        assert!(
            !harness
                .app
                .settle_notification_reveal(deadline - Duration::from_millis(1))
        );
        assert!(harness.scene().overlay.notification_reveal.is_none());
        assert!(harness.app.settle_notification_reveal(deadline));
        assert_eq!(
            harness.scene().overlay.notification_reveal.unwrap().target,
            target
        );
        assert_eq!(harness.app.state.notification_reveal.next_deadline(), None);

        harness.app.state.cursor_left_window = true;
        assert!(harness.app.settle_notification_reveal(deadline));
        assert!(harness.scene().overlay.notification_reveal.is_none());
        harness.enqueue(key_down("Tab", "Tab", 1_000));
        harness.enqueue(key_up("Tab", "Tab", 1_100));
        harness.drain();
        assert_eq!(
            harness.scene().overlay.notification_focus.as_deref(),
            Some(focus_id.as_str())
        );
        assert!(harness.app.settle_notification_reveal(deadline));
        assert_eq!(
            harness.scene().overlay.notification_reveal.unwrap().target,
            target
        );
        harness.enqueue(key_down("Escape", "Escape", 1_200));
        harness.enqueue(key_up("Escape", "Escape", 1_300));
        harness.drain();
        assert!(
            !harness
                .app
                .settle_notification_reveal(deadline + Duration::from_secs(1))
        );
        assert!(harness.scene().overlay.notification_reveal.is_none());

        let hovered = deadline + Duration::from_secs(2);
        harness.app.state.cursor_left_window = false;
        harness.app.state.cursor_x = other_source.x + other_source.width * 0.5;
        assert!(!harness.app.settle_notification_reveal(hovered));
        let dwell = hovered + Duration::from_millis(180);
        assert_eq!(
            harness.app.state.notification_reveal.next_deadline(),
            Some(dwell)
        );
        assert!(harness.app.settle_notification_reveal(dwell));
        assert_eq!(
            harness.scene().overlay.notification_reveal.unwrap().target,
            other
        );
        assert!(take_action_ids(&harness).is_empty());
    }

    #[test]
    fn reveal_wheel_replays_after_contention_without_retargeting() {
        use std::time::Instant;

        use tze_hud_scene::graph::overlay::{
            NotificationRevealField, NotificationRevealPanel, NotificationRevealRequest,
            NotificationRevealTarget,
        };
        let mut harness = approve_reject_notification_harness();
        let bounds = Rect::new(20.0, 30.0, 200.0, 120.0);
        let (panel, snapshot) = {
            let mut scene = harness.scene();
            let target = NotificationRevealTarget {
                publication_id: scene.overlay.notification_geometry[0].publication_id,
                field: NotificationRevealField::Body,
            };
            let panel = NotificationRevealPanel {
                target,
                bounds,
                bridge: Rect::new(0.0, 0.0, 0.0, 0.0),
                max_scroll: 240.0,
            };
            scene.overlay.notification_reveal_panel = Some(panel);
            scene.overlay.notification_reveal = Some(NotificationRevealRequest {
                target,
                scroll: 0.0,
            });
            (panel, crate::pipeline::HitTestSnapshot::from_scene(&scene))
        };
        harness
            .app
            .state
            .pipeline
            .hit_test_snapshot
            .store(Arc::new(snapshot));
        (harness.app.state.cursor_x, harness.app.state.cursor_y) = (50.0, 60.0);
        let scene_handle = Arc::clone(&harness.app.state.shared_state.blocking_lock().scene);
        let (dx, dy, step) = crate::windowed::input_dispatch::normalize_mouse_wheel_delta(
            &winit::event::MouseScrollDelta::LineDelta(0.0, -1.0),
            crate::os_prefs::WheelLines::Fixed,
        );
        let wake = harness.app.state.wake.clone();
        let wait_for_completion = |before| {
            let deadline = Instant::now() + SCENE_WAIT;
            while wake.main_work_generation() == before {
                assert!(
                    Instant::now() < deadline,
                    "scene availability worker did not complete for the retained wheel"
                );
                std::thread::yield_now();
            }
        };
        let held = scene_handle.blocking_lock();
        let before = wake.main_work_generation();
        harness.app.enqueue_scroll_event(dx, dy, step);
        assert_eq!(held.overlay.notification_reveal.unwrap().scroll, 0.0);
        assert!(!harness.app.drain_retained_pointer_input());
        assert_eq!(harness.app.state.pending_pointer_buttons.len(), 1);
        assert_eq!(wake.main_work_generation(), before);
        drop(held);
        // The availability worker also acquires the scene; releasing our guard
        // does not promise that an immediate try-lock can beat that worker.
        wait_for_completion(before);
        assert!(harness.app.drain_retained_pointer_input());
        assert!(harness.app.state.pending_pointer_buttons.is_empty());
        assert!(harness.app.state.pending_scroll.is_empty());
        assert_eq!(
            harness.scene().overlay.notification_reveal.unwrap().scroll,
            tze_hud_input::WHEEL_LINE_PX
        );

        let mut held = scene_handle.blocking_lock();
        let before = wake.main_work_generation();
        harness.app.enqueue_scroll_event(dx, dy, step);
        let target = NotificationRevealTarget {
            field: NotificationRevealField::Action(0),
            ..panel.target
        };
        held.overlay.notification_reveal_panel = Some(NotificationRevealPanel { target, ..panel });
        held.overlay.notification_reveal = Some(NotificationRevealRequest {
            target,
            scroll: 0.0,
        });
        assert!(!harness.app.drain_retained_pointer_input());
        assert_eq!(harness.app.state.pending_pointer_buttons.len(), 1);
        assert_eq!(wake.main_work_generation(), before);
        drop(held);
        wait_for_completion(before);
        assert!(harness.app.drain_retained_pointer_input());
        assert!(harness.app.state.pending_pointer_buttons.is_empty());
        assert!(harness.app.state.pending_scroll.is_empty());
        assert_eq!(
            harness.scene().overlay.notification_reveal.unwrap().scroll,
            0.0
        );
        assert!(take_action_ids(&harness).is_empty());
    }

    #[test]
    fn card_copy_pointer_and_answer_digit_write_locally_and_keep_the_card() {
        const TEXT: &str = "cargo test -p example\njob-42";
        for (digit, succeeded) in [(false, true), (true, true), (true, false)] {
            let mut harness = approve_reject_notification_harness();
            let id = {
                let mut scene = harness.scene();
                let card = &mut scene
                    .zone_registry
                    .active_publishes
                    .get_mut("notification-area")
                    .unwrap()[0];
                let tze_hud_scene::ZoneContent::Notification(payload) = &mut card.content else {
                    panic!("expected a notification");
                };
                payload.actions[1] = tze_hud_scene::NotificationAction {
                    label: "Copy cmd".into(),
                    copy: Some(TEXT.into()),
                    reply: None,
                    ..Default::default()
                };
                let id = card.publication_id;
                scene.version += 1;
                id
            };
            assert!(harness.tick());
            let (tx, rx) = std::sync::mpsc::channel();
            harness.app.state.notification_clipboard.write = Arc::new(move |text| {
                tx.send(text.to_owned()).unwrap();
                if succeeded {
                    Ok(())
                } else {
                    Err("fixture clipboard busy".into())
                }
            });

            if digit {
                harness
                    .app
                    .state
                    .answer_keyboard
                    .requested
                    .store(true, std::sync::atomic::Ordering::Release);
                harness.drain();
                assert!(harness.app.state.answer_keyboard.focus.is_some());
                harness.enqueue(key_down("Digit2", "2", 1_000));
                harness.enqueue(key_up("Digit2", "2", 1_100));
                harness.drain();
                assert!(harness.app.state.answer_keyboard.focus.is_none());
            } else {
                let bounds = harness
                    .scene()
                    .overlay
                    .zone_hit_regions
                    .iter()
                    .find(|r| {
                        r.publication_id == id
                            && r.kind
                                == (tze_hud_scene::types::ZoneInteractionKind::Action { index: 1 })
                    })
                    .unwrap()
                    .bounds;
                click_at(
                    &mut harness,
                    (
                        bounds.x + bounds.width * 0.5,
                        bounds.y + bounds.height * 0.5,
                    ),
                );
            }

            assert_eq!(rx.try_recv().unwrap(), TEXT);
            assert!(rx.try_recv().is_err());
            harness.app.settle_copy_feedback(std::time::Instant::now());
            {
                let scene = harness.scene();
                assert_eq!(
                    scene.zone_registry.active_for_zone("notification-area")[0].publication_id,
                    id
                );
                let result = scene.overlay.notification_copy_feedback.unwrap();
                assert_eq!(
                    (result.publication_id, result.index, result.succeeded),
                    (id, 1, succeeded)
                );
            }
            assert!(take_action_ids(&harness).is_empty());
            let until = harness.app.state.copy_feedback.unwrap().until;
            assert!(harness.app.settle_copy_feedback(until));
            assert!(harness.scene().overlay.notification_copy_feedback.is_none());
            assert!(harness.app.state.copy_feedback.is_none());
        }
    }

    fn publish_go_to_card(
        harness: &mut HeadlessEventLoopHarness,
        key: &str,
        token: &str,
    ) -> SceneId {
        let mut scene = harness.scene();
        let zone = "notification-area";
        let content = scene.zone_registry.active_for_zone(zone)[0].content.clone();
        let session = tze_hud_scene::scope::scoped_namespace("notif-agent", "go-to");
        scene.set_scoped_session_label(
            &session,
            Some(tze_hud_scene::scope::SessionLabel {
                caption: token.into(),
                window_token: Some(token.into()),
            }),
        );
        scene
            .publish_to_zone(zone, content, &session, Some(key.into()), None, None)
            .unwrap();
        scene
            .zone_registry
            .active_for_zone(zone)
            .iter()
            .find(|record| {
                record.publisher_namespace == session && record.merge_key.as_deref() == Some(key)
            })
            .unwrap()
            .publication_id
    }

    fn go_to_windows(token: &str, count: usize) -> Vec<super::super::window_match::Window> {
        use super::super::window_match::{Target, Window};
        (0..count)
            .map(|index| Window {
                target: Target {
                    hwnd: index as isize + 1,
                    pid: std::process::id() + 1,
                    minimized: false,
                },
                title: format!("{token} — editor"),
                image: "Code.exe".into(),
                visible: true,
                owned: false,
                cloaked: false,
                tool: false,
            })
            .collect()
    }

    fn go_to_receipt(harness: &HeadlessEventLoopHarness) -> Vec<SceneId> {
        harness
            .scene()
            .overlay
            .notification_geometry
            .iter()
            .filter(|card| card.has_window_caption && card.chosen.is_none())
            .map(|card| card.publication_id)
            .collect()
    }

    fn present_go_to_receipt(
        harness: &mut HeadlessEventLoopHarness,
        success: bool,
        ids: Vec<SceneId>,
    ) {
        harness
            .app
            .state
            .frame_ready_tx
            .as_ref()
            .unwrap()
            .send(true)
            .unwrap();
        harness.app.maybe_present_frame_with_secondaries(
            || Some((success, ids)),
            |_| (false, Vec::new(), Vec::new()),
        );
    }

    fn present_go_to_card(harness: &mut HeadlessEventLoopHarness, success: bool) {
        assert!(harness.tick());
        let ids = go_to_receipt(harness);
        present_go_to_receipt(harness, success, ids);
    }

    fn wait_for_go_to(
        harness: &mut HeadlessEventLoopHarness,
        predicate: impl Fn(&SceneGraph) -> bool,
    ) {
        let deadline = std::time::Instant::now() + SCENE_WAIT;
        loop {
            harness.tick();
            if predicate(&harness.scene()) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Go to never reached the expected state"
            );
            std::thread::yield_now();
        }
    }

    fn go_to_point(harness: &HeadlessEventLoopHarness, id: SceneId) -> Option<(f32, f32)> {
        harness
            .scene()
            .overlay
            .notification_geometry
            .iter()
            .find(|card| card.publication_id == id)
            .and_then(|card| card.goto)
            .map(|r| (r.x + r.width * 0.5, r.y + r.height * 0.5))
    }

    #[test]
    fn go_to_matches_only_cards_on_the_presented_texture() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let mut harness = approve_reject_notification_harness();
        let first = publish_go_to_card(&mut harness, "first-frame", "first-token");
        assert!(harness.tick());
        let first_receipt = go_to_receipt(&harness);
        assert_eq!(first_receipt, [first]);

        let next = publish_go_to_card(&mut harness, "next-frame", "next-token");
        assert!(harness.tick());
        let next_receipt = go_to_receipt(&harness);
        assert!(next_receipt.contains(&next));
        let lookups = Arc::new(AtomicUsize::new(0));
        harness.app.state.window_go_to.inject(
            {
                let lookups = lookups.clone();
                move || {
                    lookups.fetch_add(1, SeqCst);
                    go_to_windows("first-token", 1)
                        .into_iter()
                        .chain(go_to_windows("next-token", 1))
                        .collect()
                }
            },
            |_| panic!("a presented card cannot activate"),
        );

        // The renderer has built B, but only A's texture has flipped.
        present_go_to_receipt(&mut harness, true, first_receipt);
        wait_for_go_to(&mut harness, |scene| {
            scene
                .overlay
                .notification_go_to
                .get(&first)
                .is_some_and(|state| state.available)
        });
        assert!(
            !harness
                .scene()
                .overlay
                .notification_go_to
                .contains_key(&next)
        );
        assert_eq!(lookups.load(SeqCst), 1);

        present_go_to_receipt(&mut harness, true, next_receipt);
        wait_for_go_to(&mut harness, |scene| {
            scene
                .overlay
                .notification_go_to
                .get(&next)
                .is_some_and(|state| state.available)
        });
        assert_eq!(lookups.load(SeqCst), 2);
        assert_eq!(harness.app.state.window_go_to.activations, 0);
    }

    #[test]
    fn go_to_looks_up_once_after_present_and_rechecks_owner_pointer_and_keys() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
        use tze_hud_scene::graph::overlay::GoToCue;

        let token = "tzgo-7f3a";
        let clock = Arc::new(tze_hud_scene::TestClock::new(1000));
        let mut harness = approve_reject_notification_harness_on(clock.clone());
        // The answer chord chooses by publication time, then by ID. Give the
        // card under test a later timestamp than the unscoped fixture card.
        clock.advance(1);
        let card = publish_go_to_card(&mut harness, "go-to", token);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release = std::sync::Mutex::new(release_rx);
        let looks = Arc::new(AtomicUsize::new(0));
        let matches = Arc::new(AtomicUsize::new(1));
        let can_activate = Arc::new(AtomicBool::new(false));
        let activations = Arc::new(AtomicUsize::new(0));
        harness.app.state.window_go_to.inject(
            {
                let (looks, matches) = (looks.clone(), matches.clone());
                move || {
                    if looks.fetch_add(1, SeqCst) == 0 {
                        started_tx.send(()).unwrap();
                        release.lock().unwrap().recv_timeout(SCENE_WAIT).unwrap();
                    }
                    go_to_windows(token, matches.load(SeqCst))
                }
            },
            {
                let (can_activate, activations) = (can_activate.clone(), activations.clone());
                move |_| {
                    activations.fetch_add(1, SeqCst);
                    can_activate.load(SeqCst)
                }
            },
        );
        present_go_to_card(&mut harness, false);
        assert_eq!(looks.load(SeqCst), 0);
        assert!(go_to_point(&harness, card).is_none());

        present_go_to_card(&mut harness, true);
        started_rx.recv_timeout(SCENE_WAIT).unwrap();
        assert_eq!(harness.app.state.wake.counters().snapshot().presents, 1);
        assert!(
            go_to_point(&harness, card).is_none(),
            "card present before the worker answers"
        );
        release_tx.send(()).unwrap();
        wait_for_go_to(&mut harness, |scene| {
            scene
                .overlay
                .notification_geometry
                .iter()
                .any(|geometry| geometry.publication_id == card && geometry.goto.is_some())
        });
        let point = go_to_point(&harness, card).unwrap();
        assert!(
            harness
                .scene()
                .overlay
                .notification_geometry
                .iter()
                .any(|geometry| !geometry.has_window_caption && geometry.goto.is_none())
        );
        assert_eq!(activations.load(SeqCst), 0, "decoration cannot activate");

        let cue = |scene: &SceneGraph| scene.overlay.notification_go_to[&card].cue;
        matches.store(0, SeqCst);
        present_go_to_card(&mut harness, true);
        assert_eq!(go_to_point(&harness, card), Some(point));
        assert_eq!(
            looks.load(SeqCst),
            1,
            "no refresh while the card stays live"
        );
        click_at(&mut harness, point);
        click_at(&mut harness, point);
        wait_for_go_to(&mut harness, |scene| cue(scene) == Some(GoToCue::NotFound));
        assert_eq!(looks.load(SeqCst), 2, "one lookup for two close presses");
        let until = harness.app.state.window_go_to.next_deadline().unwrap();
        assert!(
            !harness
                .app
                .settle_go_to(until - std::time::Duration::from_millis(1))
        );
        assert!(harness.app.settle_go_to(until));
        assert_eq!(cue(&harness.scene()), None);

        clock.advance(2000);
        matches.store(2, SeqCst);
        click_at(&mut harness, point);
        wait_for_go_to(&mut harness, |scene| {
            cue(scene) == Some(GoToCue::Ambiguous(2))
        });
        assert_eq!(activations.load(SeqCst), 0);

        clock.advance(2000);
        matches.store(1, SeqCst);
        click_at(&mut harness, point);
        wait_for_go_to(&mut harness, |scene| cue(scene) == Some(GoToCue::Blocked));
        assert_eq!(activations.load(SeqCst), 1);
        assert_eq!(harness.app.state.window_go_to.activations, 0);

        clock.advance(2000);
        can_activate.store(true, SeqCst);
        harness
            .app
            .state
            .answer_keyboard
            .requested
            .store(true, SeqCst);
        harness.drain();
        assert_eq!(
            harness
                .app
                .state
                .answer_keyboard
                .focus
                .unwrap()
                .publication_id,
            card
        );
        let tab = |h: &mut HeadlessEventLoopHarness, reverse: bool| {
            let mut event = key_down("Tab", "Tab", 100);
            let PendingKeyboardEvent::KeyDown(raw) = &mut event else {
                unreachable!()
            };
            raw.modifiers.shift = reverse;
            h.enqueue(event);
            h.enqueue(key_up("Tab", "Tab", 110));
            h.drain();
        };
        let focus_is = |h: &HeadlessEventLoopHarness, kind| {
            let scene = h.scene();
            let focused = scene.overlay.notification_focus.as_deref();
            assert!(scene.overlay.zone_hit_regions.iter().any(|region| {
                region.publication_id == card
                    && region.kind == kind
                    && Some(region.interaction_id.as_str()) == focused
            }));
        };
        tab(&mut harness, true);
        focus_is(&harness, ZoneInteractionKind::GoTo);
        tab(&mut harness, true);
        focus_is(&harness, ZoneInteractionKind::Dismiss);
        tab(&mut harness, false);
        focus_is(&harness, ZoneInteractionKind::GoTo);
        harness.press_named_key("Enter");
        wait_for_go_to(&mut harness, |scene| {
            scene.overlay.notification_focus.is_none()
        });
        assert!(harness.app.state.answer_keyboard.focus.is_none());
        assert!(harness.app.state.answer_keyboard.return_key.is_none());
        assert_eq!(
            harness
                .app
                .state
                .surface_ui
                .lock()
                .unwrap()
                .window_activations,
            1
        );
        assert_eq!(activations.load(SeqCst), 2);
        assert!(go_to_point(&harness, card).is_some());
        assert!(
            harness.scene().pending_actions.is_empty(),
            "Go to never queues agent input"
        );
    }

    #[test]
    fn go_to_drops_a_replaced_press_while_another_card_rechecks() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        use tze_hud_scene::graph::overlay::GoToCue;

        let mut harness = approve_reject_notification_harness();
        let a = publish_go_to_card(&mut harness, "first", "first-token");
        let b = publish_go_to_card(&mut harness, "second", "second-token");
        let looks = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release = std::sync::Mutex::new(release_rx);
        harness.app.state.window_go_to.inject(
            {
                let looks = looks.clone();
                move || match looks.fetch_add(1, SeqCst) {
                    0 => go_to_windows("first-token", 1)
                        .into_iter()
                        .chain(go_to_windows("second-token", 1))
                        .collect(),
                    1 => {
                        started_tx.send(()).unwrap();
                        release.lock().unwrap().recv_timeout(SCENE_WAIT).unwrap();
                        go_to_windows("first-token", 1)
                    }
                    _ => Vec::new(),
                }
            },
            |_| panic!("neither surviving lookup may activate a target"),
        );
        present_go_to_card(&mut harness, true);
        wait_for_go_to(&mut harness, |scene| {
            [a, b].iter().all(|id| {
                scene
                    .overlay
                    .notification_geometry
                    .iter()
                    .any(|card| card.publication_id == *id && card.goto.is_some())
            })
        });
        let (first, second) = (
            go_to_point(&harness, a).unwrap(),
            go_to_point(&harness, b).unwrap(),
        );
        click_at(&mut harness, first);
        started_rx.recv_timeout(SCENE_WAIT).unwrap();
        click_at(&mut harness, second);
        wait_for_go_to(&mut harness, |scene| {
            scene.overlay.notification_go_to[&b].cue == Some(GoToCue::NotFound)
        });
        assert_eq!(
            looks.load(SeqCst),
            3,
            "the other card runs its own press lookup"
        );

        let replacement = publish_go_to_card(&mut harness, "first", "first-token");
        assert_ne!(replacement, a);
        release_tx.send(()).unwrap();
        let end = std::time::Instant::now() + SCENE_WAIT;
        while harness.app.state.window_go_to.press_pending_for_test() {
            harness.tick();
            assert!(std::time::Instant::now() < end);
            std::thread::yield_now();
        }
        assert_eq!(harness.app.state.window_go_to.activations, 0);
        assert!(!harness.scene().overlay.notification_go_to.contains_key(&a));
    }

    #[test]
    fn go_to_cancels_a_pending_switch_on_nested_escape_or_hidden_card() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        use tze_hud_scene::graph::{SurfaceClasses, overlay::GoToCue};

        for hide in [false, true] {
            let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
            let mut harness = approve_reject_notification_harness_on(clock.clone());
            let (zone, content) = {
                let scene = harness.scene();
                (
                    scene.zone_registry.zones["notification-area"].clone(),
                    scene.zone_registry.active_for_zone("notification-area")[0]
                        .content
                        .clone(),
                )
            };
            harness.focus_composer();
            {
                let mut scene = harness.scene();
                scene.register_zone(zone);
                scene
                    .publish_to_zone(
                        "notification-area",
                        content,
                        "notif-agent",
                        None,
                        None,
                        None,
                    )
                    .unwrap();
            }
            // The owner chord must select the scoped card over the generic fixture.
            clock.advance(1);
            let card = publish_go_to_card(&mut harness, "go-to", "nested");
            let pairs = Arc::new(AtomicUsize::new(0));
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let release = std::sync::Mutex::new(release_rx);
            harness.app.state.window_go_to.inject(
                move || {
                    if pairs.fetch_add(1, SeqCst) == 1 {
                        started_tx.send(()).unwrap();
                        release.lock().unwrap().recv_timeout(SCENE_WAIT).unwrap();
                    }
                    go_to_windows("nested", 1)
                },
                |_| panic!("cancelled owner intent cannot activate"),
            );
            // The real foreground remains the same HWND when nested Esc
            // leaves the card and returns to a composer in the HUD.
            harness.app.state.window_go_to.test_foreground = Some(Box::new(|| Some(10)));
            harness.app.state.window_go_to.test_hud_foreground = Some(10);
            present_go_to_card(&mut harness, true);
            wait_for_go_to(&mut harness, |scene| {
                scene
                    .overlay
                    .notification_go_to
                    .get(&card)
                    .is_some_and(|state| state.available)
            });

            if hide {
                let point = go_to_point(&harness, card).unwrap();
                click_at(&mut harness, point);
            } else {
                assert_eq!(harness.app.focus_newest_question(true), Ok(true));
                let focus = harness.app.state.answer_keyboard.focus.unwrap();
                assert_eq!(focus.publication_id, card);
                assert!(focus.nested);
                let mut step = key_down("Tab", "Tab", 100);
                let PendingKeyboardEvent::KeyDown(raw) = &mut step else {
                    unreachable!()
                };
                raw.modifiers.shift = true;
                harness.enqueue(step);
                harness.enqueue(key_up("Tab", "Tab", 110));
                harness.drain();
                harness.press_named_key("Enter");
            }
            started_rx.recv_timeout(SCENE_WAIT).unwrap();
            assert_eq!(
                harness.scene().overlay.notification_go_to[&card].cue,
                Some(GoToCue::Switching)
            );
            if hide {
                harness.scene().set_owner_hidden(SurfaceClasses {
                    zones: true,
                    ..SurfaceClasses::NONE
                });
                assert!(harness.tick());
                assert!(go_to_point(&harness, card).is_none());
            } else {
                harness.press_named_key("Escape");
                assert!(harness.app.state.answer_keyboard.focus.is_none());
                assert!(harness.composer_draft().is_some());
            }
            release_tx.send(()).unwrap();
            wait_for_go_to(&mut harness, |scene| {
                scene.overlay.notification_go_to[&card].cue.is_none()
            });
            if hide {
                harness.scene().set_owner_hidden(SurfaceClasses::NONE);
                assert!(harness.tick());
                assert!(go_to_point(&harness, card).is_some());
            }
            assert_eq!(harness.app.state.window_go_to.activations, 0);
            assert!(harness.scene().pending_actions.is_empty());
        }
    }

    #[test]
    fn go_to_does_not_rebase_retained_pointer_consent_onto_a_new_foreground() {
        use super::super::lifecycle::ChromeCaptureTestScope;
        use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering::SeqCst};

        let mut harness = approve_reject_notification_harness();
        let card = publish_go_to_card(&mut harness, "go-to", "retained");
        let lookups = Arc::new(AtomicUsize::new(0));
        harness.app.state.window_go_to.inject(
            {
                let lookups = lookups.clone();
                move || {
                    lookups.fetch_add(1, SeqCst);
                    go_to_windows("retained", 1)
                }
            },
            |_| panic!("a stale click cannot activate"),
        );
        let foreground = Arc::new(AtomicIsize::new(30));
        harness.app.state.window_go_to.test_foreground = Some(Box::new({
            let foreground = foreground.clone();
            move || Some(foreground.load(SeqCst))
        }));
        present_go_to_card(&mut harness, true);
        wait_for_go_to(&mut harness, |scene| {
            scene
                .overlay
                .notification_go_to
                .get(&card)
                .is_some_and(|state| state.available)
        });
        let point = go_to_point(&harness, card).unwrap();

        let scene_handle = Arc::clone(&harness.app.state.shared_state.blocking_lock().scene);
        let held = scene_handle.blocking_lock();
        (harness.app.state.cursor_x, harness.app.state.cursor_y) = point;
        let window = winit::window::WindowId::dummy();
        let native = ChromeCaptureTestScope::new(window);
        for down in [true, false] {
            let receipt = native.begin(down);
            harness
                .app
                .handle_mouse_button(winit::event::MouseButton::Left, down, window, None);
            native.complete(receipt);
        }
        assert_eq!(harness.app.state.pending_pointer_buttons.len(), 2);
        foreground.store(31, SeqCst);
        drop(held);
        let deadline = std::time::Instant::now() + SCENE_WAIT;
        while !harness.tick() || !harness.app.state.pending_pointer_buttons.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "retained pointer did not settle"
            );
            std::thread::yield_now();
        }
        let state = &harness.scene().overlay.notification_go_to[&card];
        assert_eq!(state.attempted_at_mono_us, None);
        assert_eq!(state.cue, None);
        assert_eq!(lookups.load(SeqCst), 1);
    }

    #[test]
    fn go_to_drops_a_retained_answer_key_after_native_foreground_leaves_the_hud() {
        use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering::SeqCst};
        use tze_hud_scene::graph::overlay::GoToCue;

        let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
        let mut harness = approve_reject_notification_harness_on(clock.clone());
        clock.advance(1);
        let card = publish_go_to_card(&mut harness, "go-to", "retained-key");
        let lookups = Arc::new(AtomicUsize::new(0));
        harness.app.state.window_go_to.inject(
            {
                let lookups = lookups.clone();
                move || {
                    lookups.fetch_add(1, SeqCst);
                    go_to_windows("retained-key", 1)
                }
            },
            |_| false,
        );
        let foreground = Arc::new(AtomicIsize::new(30));
        harness.app.state.window_go_to.test_foreground = Some(Box::new({
            let foreground = foreground.clone();
            move || Some(foreground.load(SeqCst))
        }));
        harness.app.state.window_go_to.test_hud_foreground = Some(30);
        present_go_to_card(&mut harness, true);
        wait_for_go_to(&mut harness, |scene| {
            scene.overlay.notification_go_to[&card].available
        });
        assert_eq!(harness.app.focus_newest_question(false), Ok(true));
        assert_eq!(
            harness
                .app
                .state
                .answer_keyboard
                .focus
                .unwrap()
                .publication_id,
            card
        );
        let mut step = key_down("Tab", "Tab", 100);
        let PendingKeyboardEvent::KeyDown(raw) = &mut step else {
            unreachable!()
        };
        raw.modifiers.shift = true;
        harness.enqueue(step);
        harness.enqueue(key_up("Tab", "Tab", 110));
        harness.drain();

        let handle = Arc::clone(&harness.app.state.shared_state.blocking_lock().scene);
        let held = handle.blocking_lock();
        let PendingKeyboardEvent::KeyDown(enter) = key_down("Enter", "Enter", 200) else {
            unreachable!()
        };
        let PendingKeyboardEvent::KeyUp(release) = key_up("Enter", "Enter", 210) else {
            unreachable!()
        };
        harness.app.dispatch_key_down_event(&enter);
        harness.app.dispatch_key_up_event(&release);
        assert_eq!(harness.pending_len(), 2);
        // Windows has moved on, but the deferred Enter drains before winit
        // receives the corresponding focus-loss event.
        foreground.store(31, SeqCst);
        drop(held);
        harness.drain();
        assert_eq!(harness.pending_len(), 0);
        assert_eq!(
            harness
                .app
                .state
                .answer_keyboard
                .focus
                .unwrap()
                .publication_id,
            card
        );
        assert_eq!(
            harness.scene().overlay.notification_go_to[&card].attempted_at_mono_us,
            None
        );
        assert_eq!(lookups.load(SeqCst), 1);

        // A fresh press while the HUD owns foreground still reaches the
        // press lookup. The fake Windows API refuses the actual switch.
        foreground.store(30, SeqCst);
        harness.press_named_key("Enter");
        wait_for_go_to(&mut harness, |scene| {
            scene.overlay.notification_go_to[&card].cue == Some(GoToCue::Blocked)
        });
        assert_eq!(lookups.load(SeqCst), 2);
        assert_eq!(harness.app.state.window_go_to.activations, 0);
    }

    #[test]
    fn go_to_counts_success_only_after_observing_the_destination_foreground() {
        use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::SeqCst};
        use tze_hud_scene::graph::overlay::GoToCue;

        let mut harness = approve_reject_notification_harness();
        let card = publish_go_to_card(&mut harness, "go-to", "observed");
        let requested = Arc::new(AtomicBool::new(false));
        let foreground = Arc::new(AtomicIsize::new(30));
        harness
            .app
            .state
            .window_go_to
            .inject(|| go_to_windows("observed", 1), {
                let requested = requested.clone();
                move |_| {
                    requested.store(true, SeqCst);
                    true
                }
            });
        harness.app.state.window_go_to.test_foreground = Some(Box::new({
            let foreground = foreground.clone();
            move || Some(foreground.load(SeqCst))
        }));
        present_go_to_card(&mut harness, true);
        wait_for_go_to(&mut harness, |scene| {
            scene
                .overlay
                .notification_go_to
                .get(&card)
                .is_some_and(|state| state.available)
        });
        let point = go_to_point(&harness, card).unwrap();
        click_at(&mut harness, point);
        wait_for_go_to(&mut harness, |_| requested.load(SeqCst));
        assert_eq!(harness.app.state.window_go_to.activations, 0);
        assert_eq!(
            harness.scene().overlay.notification_go_to[&card].cue,
            Some(GoToCue::Switching)
        );

        let check = harness.app.state.window_go_to.next_deadline().unwrap();
        foreground.store(1, SeqCst);
        harness.app.settle_go_to(check);
        assert_eq!(harness.app.state.window_go_to.activations, 1);
        wait_for_go_to(&mut harness, |scene| {
            scene.overlay.notification_go_to[&card].cue.is_none()
        });
        assert!(harness.scene().pending_actions.is_empty());
    }

    /// Settle until the pressed link shows `state` (the worker reports on its
    /// own thread), returning what the scene and the admin mirror show.
    fn settle_link_to(
        harness: &mut HeadlessEventLoopHarness,
        state: tze_hud_scene::LinkFeedbackState,
    ) -> (tze_hud_scene::LinkFeedbackTarget, Option<&'static str>) {
        let deadline = std::time::Instant::now() + SCENE_WAIT;
        loop {
            harness.tick();
            if let Some((target, shown)) = harness.scene().overlay.link_feedback.clone()
                && shown == state
            {
                let admin = harness.app.state.surface_ui.lock().unwrap().link_feedback;
                return (target, admin);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "link never showed {state:?}"
            );
            std::thread::yield_now();
        }
    }

    /// R3.2 S1, S2, S4, S5: a pressed card link says "Opening…" at once, then
    /// what the worker reported for `link.feedback.ms`, then its label again;
    /// a refused repeat changes nothing; a worker that cannot start fails at
    /// once without counting; removal takes the feedback with the card.
    /// Nothing reaches the agent and the card is unchanged.
    #[test]
    fn card_link_feedback_shows_opening_then_the_worker_result() {
        use tze_hud_scene::LinkFeedbackState::{Failed, Opened, Opening};
        use tze_hud_scene::LinkFeedbackTarget::CardAction;
        let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
        let (mut harness, verdict, points) = link_card_harness(clock.clone());
        harness
            .app
            .state
            .global_tokens
            .insert("link.feedback.ms".into(), "1500".into());
        let card = harness
            .scene()
            .zone_registry
            .active_for_zone("notification-area")[0]
            .clone();
        let publication_id = card.publication_id;
        let launches = harness.app.state.notification_link_opener.launches.clone();

        click_at(&mut harness, points[0]);
        let (target, admin) = settle_link_to(&mut harness, Opening);
        assert_eq!(
            target,
            CardAction {
                publication_id,
                index: 0
            }
        );
        assert_eq!(admin, Some("opening"));
        assert_eq!(
            harness.app.state.link_feedback.as_ref().unwrap().until,
            None,
            "an open handoff arms no deadline"
        );
        // S4: a repeat inside the 2 s limit neither relaunches nor restarts.
        let workers = harness.app.state.notification_link_opener.workers.len();
        click_at(&mut harness, points[0]);
        assert_eq!(
            harness.app.state.notification_link_opener.workers.len(),
            workers
        );
        assert_eq!(launches.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(settle_link_to(&mut harness, Opening).0, target);

        verdict.send(Ok(())).unwrap();
        assert_eq!(
            settle_link_to(&mut harness, Opened),
            (target.clone(), Some("opened"))
        );
        let until = harness
            .app
            .state
            .link_feedback
            .as_ref()
            .unwrap()
            .until
            .unwrap();
        assert!(
            !harness
                .app
                .settle_link_feedback(until - std::time::Duration::from_millis(1))
        );
        assert!(
            harness.app.settle_link_feedback(until),
            "the result ends at its deadline"
        );
        assert_eq!(harness.scene().overlay.link_feedback, None);
        assert!(harness.app.state.link_feedback.is_none());
        assert_eq!(
            harness.app.state.surface_ui.lock().unwrap().link_feedback,
            None
        );

        // S2: the worker fails.
        click_at(&mut harness, points[1]);
        settle_link_to(&mut harness, Opening);
        verdict.send(Err("injected".into())).unwrap();
        assert_eq!(
            settle_link_to(&mut harness, Failed),
            (
                CardAction {
                    publication_id,
                    index: 1
                },
                Some("failed")
            )
        );
        // S2: no worker starts: "Couldn't open" at once, never "Opening…".
        clock.advance(2000);
        harness.app.state.notification_link_opener.fail_spawn = true;
        click_at(&mut harness, points[0]);
        harness.app.state.notification_link_opener.fail_spawn = false;
        let feedback = harness.app.state.link_feedback.as_ref().unwrap();
        assert_eq!(
            (feedback.state, feedback.target.clone()),
            (Failed, target.clone())
        );
        assert!(feedback.until.is_some());
        assert_eq!(launches.load(std::sync::atomic::Ordering::Relaxed), 2);
        harness.tick();
        assert_eq!(
            harness.scene().overlay.link_feedback,
            Some((target.clone(), Failed))
        );

        // S5: the card leaves during "Opening…": the feedback goes with it.
        clock.advance(2000);
        click_at(&mut harness, points[0]);
        settle_link_to(&mut harness, Opening);
        assert!(harness.scene().dismiss_notification(
            "notification-area",
            publication_id,
            &card.publisher_namespace
        ));
        harness.tick();
        assert!(harness.app.state.link_feedback.is_none());
        assert_eq!(harness.scene().overlay.link_feedback, None);
        assert!(
            take_action_ids(&harness).is_empty(),
            "nothing reaches the agent"
        );
        drop(verdict);
        for worker in harness.app.state.notification_link_opener.workers.drain(..) {
            worker.join().unwrap();
        }
    }

    /// R3.2 S7: a worker that never returns keeps "Opening…" with no deadline,
    /// so nothing wakes the HUD until the control leaves.
    #[test]
    fn link_worker_that_never_returns_arms_no_wake() {
        let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
        let (mut harness, verdict, points) = link_card_harness(clock);
        click_at(&mut harness, points[0]);
        settle_link_to(&mut harness, tze_hud_scene::LinkFeedbackState::Opening);
        for _ in 0..3 {
            harness.tick();
        }
        let later = std::time::Instant::now() + std::time::Duration::from_secs(3600);
        harness.app.settle_link_feedback(later);
        let feedback = harness.app.state.link_feedback.as_ref().unwrap();
        assert_eq!(feedback.state, tze_hud_scene::LinkFeedbackState::Opening);
        assert_eq!(feedback.until, None, "no deadline: no wake is armed");
        assert!(feedback.drawn, "nothing is waiting on the scene either");
        drop(verdict);
        for worker in harness.app.state.notification_link_opener.workers.drain(..) {
            worker.join().unwrap();
        }
    }

    /// One click on Approve queues exactly one `approve` and dismisses the
    /// notification, so the click has an immediate visible result and a
    /// second click has nothing to land on.
    #[test]
    fn action_click_queues_one_action_and_dismisses_the_notification() {
        // The existing real pointer route also owns local links, exact debounce
        // identity and failure handling. Native receipts authorize ordinary edges.
        {
            use super::super::lifecycle::ChromeCaptureTestScope;
            use std::sync::atomic::{AtomicUsize, Ordering};
            use std::sync::mpsc;
            use std::time::Duration;
            use tze_hud_scene::{NotificationAction, ZoneContent};
            use winit::event::MouseButton;
            let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
            let mut harness = approve_reject_notification_harness_on(clock.clone());
            let urls = ["https://éxample.com/a\"b", "https://example.com/second"];
            {
                let mut scene = harness.scene();
                let record = &mut scene
                    .zone_registry
                    .active_publishes
                    .get_mut("notification-area")
                    .unwrap()[0];
                let ZoneContent::Notification(payload) = &mut record.content else {
                    unreachable!()
                };
                payload.actions[0] = NotificationAction {
                    copy: None,
                    reply: None,
                    label: "Docs".into(),
                    callback_id: None,
                    url: Some(urls[0].into()),
                    role: None,
                };
                payload.actions[1] = NotificationAction {
                    copy: None,
                    reply: None,
                    label: "More".into(),
                    callback_id: None,
                    url: Some(urls[1].into()),
                    role: None,
                };
                payload.actions[2].callback_id = Some("approve".into());
                payload.actions[3].callback_id = Some("approve".into());
            }
            assert!(harness.tick());
            let (tx, rx) = mpsc::channel();
            harness.app.state.notification_link_opener.worker = Some(Arc::new(move |url| {
                tx.send((url, std::thread::current().id())).unwrap();
                Err("injected opener failure".into())
            }));
            let wakes = Arc::new(AtomicUsize::new(0));
            let observed = wakes.clone();
            harness.scene().register_action_notifier(
                "notif-agent",
                tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
                    observed.fetch_add(1, Ordering::SeqCst);
                }),
            );
            let window = winit::window::WindowId::dummy();
            let mut native = ChromeCaptureTestScope::new(window);
            let click = |h: &mut HeadlessEventLoopHarness, point: (f32, f32)| {
                (h.app.state.cursor_x, h.app.state.cursor_y) = point;
                for down in [true, false] {
                    let receipt = native.begin(down);
                    h.app
                        .handle_mouse_button(MouseButton::Left, down, window, None);
                    native.complete(receipt);
                }
                h.drain();
            };
            let points = urls.map(|url| harness.notification_link_center(url).unwrap());
            let original = harness
                .scene()
                .zone_registry
                .active_for_zone("notification-area")[0]
                .clone();
            click(&mut harness, points[0]);
            let (url, thread) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(url, "https://xn--xample-9ua.com/a%22b");
            assert_ne!(thread, std::thread::current().id());
            let launches = harness.app.state.notification_link_opener.launches.clone();
            assert_eq!(launches.load(Ordering::Relaxed), 1);
            for elapsed in [0, 1999] {
                let before = harness.app.state.notification_link_opener.workers.len();
                clock.advance(elapsed);
                click(&mut harness, points[0]);
                assert_eq!(
                    harness.app.state.notification_link_opener.workers.len(),
                    before
                );
                assert!(rx.try_recv().is_err());
                assert_eq!(launches.load(Ordering::Relaxed), 1);
            }
            clock.advance(1);
            click(&mut harness, points[0]);
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().0, url);
            click(&mut harness, points[1]);
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().0, urls[1]);
            assert_eq!(launches.load(Ordering::Relaxed), 3);
            assert_eq!(
                harness
                    .scene()
                    .zone_registry
                    .active_for_zone("notification-area")[0],
                original
            );
            assert!(harness.scene().overlay.notification_choices.is_empty());
            assert!(take_action_ids(&harness).is_empty());
            assert_eq!(
                wakes.load(Ordering::SeqCst),
                0,
                "links create no input readiness"
            );
            // Failed spawn still stamps the accepted attempt, before dispatch.
            clock.advance(2000);
            let before = harness.app.state.notification_link_opener.workers.len();
            harness.app.state.notification_link_opener.fail_spawn = true;
            click(&mut harness, points[0]);
            harness.app.state.notification_link_opener.fail_spawn = false;
            click(&mut harness, points[0]);
            assert_eq!(
                harness.app.state.notification_link_opener.workers.len(),
                before
            );
            assert!(rx.try_recv().is_err());
            assert_eq!(
                launches.load(Ordering::Relaxed),
                3,
                "failed spawn and refused retry"
            );
            clock.advance(2000);
            click(&mut harness, points[0]);
            rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(launches.load(Ordering::Relaxed), 4);
            #[cfg(not(windows))]
            {
                let mut real_noop = crate::shell::link::LinkOpener::default();
                real_noop.launch("https://example.invalid/".into());
                for worker in real_noop.workers.drain(..) {
                    worker.join().unwrap();
                }
                assert_eq!(real_noop.launches.load(Ordering::Relaxed), 1);
            }
            // Lost native Up, focus cancellation and safe mode never open links.
            for denied in ["focus", "safe-mode", "native-up", "native-mismatched-up"] {
                let workers_before = harness.app.state.notification_link_opener.workers.len();
                clock.advance(2000);
                (harness.app.state.cursor_x, harness.app.state.cursor_y) = points[0];
                if denied == "safe-mode" {
                    harness
                        .app
                        .state
                        .safe_mode_atomic
                        .store(true, Ordering::SeqCst);
                }
                let receipt = native.begin(true);
                harness
                    .app
                    .handle_mouse_button(MouseButton::Left, true, window, None);
                native.complete(receipt);
                if denied == "focus" {
                    harness.app.handle_window_focus(None, false);
                }
                let receipt = if denied == "native-up" {
                    None
                } else {
                    native.begin(denied == "native-mismatched-up")
                };
                harness
                    .app
                    .handle_mouse_button(MouseButton::Left, false, window, None);
                native.complete(receipt);
                harness.drain();
                assert_eq!(
                    harness.app.state.notification_link_opener.workers.len(),
                    workers_before,
                    "{denied} must spawn no worker"
                );
                assert!(rx.try_recv().is_err(), "{denied}");
                harness
                    .app
                    .state
                    .safe_mode_atomic
                    .store(false, Ordering::SeqCst);
                if denied == "focus" {
                    harness.app.handle_window_focus(None, true);
                }
                if denied.starts_with("native-") {
                    assert!(
                        !native.healthy(),
                        "denied delivery faults its native ledger"
                    );
                    // Start an independent injected native lifetime for the next
                    // case; production never rearms a faulted installation.
                    drop(native);
                    native = super::super::lifecycle::ChromeCaptureTestScope::new(window);
                }
            }
            for worker in harness.app.state.notification_link_opener.workers.drain(..) {
                worker.join().unwrap();
            }
            drop(native);
            // Exact publication cleanup, not a reusable key or callback string.
            let stale = harness
                .scene()
                .overlay
                .zone_hit_regions
                .iter()
                .find(|r| matches!(r.kind, ZoneInteractionKind::Action { index: 0 }))
                .unwrap()
                .clone();
            let content = original.content.clone();
            harness.scene().clear_zone("notification-area").unwrap();
            assert!(
                harness
                    .scene()
                    .overlay
                    .notification_link_attempts
                    .is_empty()
            );
            harness
                .scene()
                .publish_to_zone(
                    "notification-area",
                    content,
                    "notif-agent",
                    None,
                    None,
                    None,
                )
                .unwrap();
            // The pointer is away while it arrives, so no press guard applies.
            (harness.app.state.cursor_x, harness.app.state.cursor_y) = (0.0, 0.0);
            assert!(harness.tick());
            assert!(harness.scene().activate_notification(&stale, 0).is_none());
            let point = harness.notification_link_center(urls[0]).unwrap();
            harness.click(point.0, point.1);
            harness.drain();
            rx.recv_timeout(Duration::from_secs(2)).unwrap();
            // Equal callback text does not alias distinct index-derived controls.
            let ids: Vec<_> = harness
                .scene()
                .overlay
                .zone_hit_regions
                .iter()
                .filter(|r| matches!(r.kind, ZoneInteractionKind::Action { .. }))
                .map(|r| r.interaction_id.clone())
                .collect();
            assert_eq!(ids.len(), 4);
            assert_ne!(ids[2], ids[3]);
            let point = harness.notification_action_center("approve").unwrap();
            harness.click(point.0, point.1);
            harness.drain();
            assert_eq!(take_action_ids(&harness), ["approve"]);
            assert_eq!(wakes.load(Ordering::SeqCst), 1);
            assert!(
                harness
                    .scene()
                    .overlay
                    .notification_link_attempts
                    .is_empty()
            );
            for worker in harness.app.state.notification_link_opener.workers.drain(..) {
                worker.join().unwrap();
            }
        }
        // Native callbacks surround delivery to winit. Panel-owned clicks must
        // consume those same receipts even though they never reach the scene.
        // Reuse the real panel task/factory and compositor-generated geometry.
        {
            use crate::shell::control_panel::PanelEvent;
            use crate::windowed::lifecycle::ChromeCaptureTestScope;
            use std::time::{Duration, Instant};
            use tze_hud_scene::config::ChordRole;
            use winit::event::MouseButton;

            let runtime = tokio::runtime::Runtime::new().unwrap();
            let _entered = runtime.enter();
            let panel_fixture = super::super::panel_input::panel_task_tests::fixture();
            let panel = panel_fixture.panel.clone();
            let mut harness = approve_reject_notification_harness();
            harness
                .app
                .state
                .global_tokens
                .insert("motion.exit.ms".into(), "120".into());
            harness.app.state.control_panel = Some(panel.clone());
            assert!(panel.send(PanelEvent::Chord(ChordRole::Panel)));
            let deadline = Instant::now() + Duration::from_secs(2);
            while panel.current_model().is_none() {
                assert!(Instant::now() < deadline, "panel opened");
                std::thread::sleep(Duration::from_millis(1));
            }
            let geometry = tze_hud_compositor::estimate_control_panel_geometry(
                &Default::default(),
                &panel.current_model().unwrap(),
                1920.0,
                1080.0,
            );
            let background = geometry.panel;
            panel.set_geometry(panel.open_visit().unwrap(), 1, || geometry.clone());
            // The top-left inset is background, outside every row/control.
            (harness.app.state.cursor_x, harness.app.state.cursor_y) =
                (background.x + 1.0, background.y + 1.0);
            assert_eq!(
                panel.hit(harness.app.state.cursor_x, harness.app.state.cursor_y),
                Some(tze_hud_compositor::PanelHitTarget::Panel)
            );
            let window = winit::window::WindowId::dummy();
            let native = ChromeCaptureTestScope::new(window);
            for down in [true, false] {
                let frame = native.begin(down);
                harness
                    .app
                    .handle_mouse_button(MouseButton::Left, down, window, None);
                native.complete(frame);
                assert!(!harness.app.state.left_button_down, "panel owns the edge");
            }
            assert!(
                take_action_ids(&harness).is_empty(),
                "panel creates no callback"
            );
            assert!(panel.send(PanelEvent::Chord(ChordRole::Panel)));
            let deadline = Instant::now() + Duration::from_secs(2);
            while panel.is_open() {
                assert!(Instant::now() < deadline, "panel closed");
                std::thread::sleep(Duration::from_millis(1));
            }
            // Match the owner sequence: change exit motion live, then press a
            // generated notification button. This is still the same publication.
            harness
                .app
                .state
                .global_tokens
                .insert("motion.exit.ms".into(), "0".into());
            let (x, y) = harness.notification_action_center("approve").unwrap();
            (harness.app.state.cursor_x, harness.app.state.cursor_y) = (x, y);
            for down in [true, false] {
                let frame = native.begin(down);
                harness
                    .app
                    .handle_mouse_button(MouseButton::Left, down, window, None);
                native.complete(frame);
                if down {
                    assert!(chrome_state(&harness, "approve").pressed, "held feedback");
                }
            }
            assert!(
                native.healthy(),
                "panel routing must not poison capture history"
            );
            assert_eq!(take_action_ids(&harness), ["approve"]);
            assert_eq!(harness.zone_publication_count("notification-area"), 0);
            let frame = native.begin(false);
            harness
                .app
                .handle_mouse_button(MouseButton::Left, false, window, None);
            native.complete(frame);
            assert!(
                take_action_ids(&harness).is_empty(),
                "late Up never repeats"
            );
        }

        let mut harness = approve_reject_notification_harness();
        let (x, y) = harness.notification_action_center("approve").unwrap();
        harness.click(x, y);
        harness.drain();
        assert_eq!(take_action_ids(&harness), ["approve"]);
        assert_eq!(harness.zone_publication_count("notification-area"), 0);
        {
            let shared = harness.app.state.shared_state.blocking_lock();
            let scene = shared.scene.blocking_lock();
            assert!(
                scene.overlay.zone_hit_regions.is_empty(),
                "the answered notification leaves no clickable buttons"
            );
            assert!(
                scene.overlay.chrome_states.is_empty(),
                "the dismissed buttons leave no hover/press state"
            );
        }

        // The same spot again: nothing left to activate.
        harness.click(x, y);
        harness.drain();
        assert!(take_action_ids(&harness).is_empty());
        for terminal in ["expiry", "clear", "replacement", "dismiss", "hidden"] {
            let mut harness = approve_reject_notification_harness();
            harness.app.state.global_tokens =
                tze_hud_config::themes::resolve_config_tokens(&std::collections::HashMap::from([
                    ("theme".into(), "blueprint".into()),
                    ("motion.exit.ms".into(), "240".into()),
                ]));
            let region = harness
                .scene()
                .overlay
                .zone_hit_regions
                .iter()
                .find(|r| matches!(&r.kind, ZoneInteractionKind::Action { index: 0 }))
                .unwrap()
                .clone();
            harness.click(
                region.bounds.x + region.bounds.width / 2.0,
                region.bounds.y + region.bounds.height / 2.0,
            );
            assert_eq!(take_action_ids(&harness), ["approve"]);
            {
                let mut scene = harness.scene();
                assert!(scene.overlay.zone_hit_regions.is_empty());
                assert!(scene.overlay.chrome_states[&region.interaction_id].pressed);
                assert!(scene.overlay.notification_focus.is_none());
                assert!(
                    scene.activate_notification(&region, 240).is_none(),
                    "already answered"
                );
                assert!(
                    !scene.hold_zone_publications("notification-area", "notif-agent", None),
                    "hold cannot prolong local exit"
                );
                match terminal {
                    "expiry" => {
                        scene
                            .zone_registry
                            .active_publishes
                            .get_mut("notification-area")
                            .unwrap()[0]
                            .expires_at_wall_us = Some(0);
                        scene.drain_expired_zone_publications();
                    }
                    "clear" | "replacement" => {
                        let old =
                            scene.zone_registry.active_for_zone("notification-area")[0].clone();
                        scene.clear_zone("notification-area").unwrap();
                        if terminal == "replacement" {
                            scene
                                .publish_to_zone(
                                    "notification-area",
                                    old.content,
                                    "notif-agent",
                                    None,
                                    None,
                                    None,
                                )
                                .unwrap();
                        }
                    }
                    "dismiss" => {
                        assert!(scene.dismiss_notification(
                            "notification-area",
                            region.publication_id,
                            "notif-agent"
                        ));
                    }
                    "hidden" => {
                        scene.set_owner_hidden(tze_hud_scene::graph::visibility::SurfaceClasses {
                            zones: true,
                            ..Default::default()
                        });
                    }
                    _ => unreachable!(),
                }
                if terminal == "hidden" {
                    assert!(scene.overlay.zone_hit_regions.is_empty());
                    assert_eq!(
                        scene
                            .zone_registry
                            .active_for_zone("notification-area")
                            .len(),
                        1,
                        "hiding changes the view, not publications"
                    );
                    scene
                        .zone_registry
                        .active_publishes
                        .get_mut("notification-area")
                        .unwrap()[0]
                        .expires_at_wall_us = Some(0);
                    scene.drain_expired_zone_publications();
                }
                assert!(scene.overlay.notification_choices.is_empty());
                assert!(
                    !scene
                        .overlay
                        .chrome_states
                        .contains_key(&region.interaction_id)
                );
                assert!(
                    scene.take_pending_actions("notif-agent").is_empty(),
                    "automatic cleanup emits neither Callback nor human X"
                );
                assert!(scene.take_pending_actions("another-agent").is_empty());
            }
            harness.app.enqueue_pointer_event(PointerEventKind::Up);
            harness.drain();
            assert!(take_action_ids(&harness).is_empty());
        }

        // Same publisher, same fixed clock tick: each card owns its callback,
        // controls and finite exit independently. No timestamp adjustment.
        let mut harness = approve_reject_notification_harness();
        let first = harness
            .scene()
            .zone_registry
            .active_for_zone("notification-area")[0]
            .clone();
        let mut second_content = first.content.clone();
        if let tze_hud_scene::ZoneContent::Notification(payload) = &mut second_content {
            payload.actions[0].callback_id = Some("second".into());
            payload.actions[0].label = "Second card".into();
        }
        harness
            .scene()
            .publish_to_zone(
                "notification-area",
                second_content.clone(),
                "notif-agent",
                None,
                None,
                None,
            )
            .unwrap();
        assert!(harness.tick());
        let second = harness
            .scene()
            .overlay
            .zone_hit_regions
            .iter()
            .find(|r| matches!(&r.kind, ZoneInteractionKind::Action { index: 0 }))
            .unwrap()
            .clone();
        {
            let mut scene = harness.scene();
            let records = scene.zone_registry.active_for_zone("notification-area");
            assert_eq!(records.len(), 2);
            assert_eq!(
                records[0].published_at_wall_us,
                records[1].published_at_wall_us
            );
            assert_ne!(records[0].publication_id, records[1].publication_id);
            // Snapshot format carries no runtime identity; restoration gives
            // every record a distinct fresh identity at the unchanged time.
            let json = serde_json::to_string(records).unwrap();
            assert!(!json.contains("publication_id"));
            let restored: Vec<tze_hud_scene::ZonePublishRecord> =
                serde_json::from_str(&json).unwrap();
            assert_ne!(restored[0].publication_id, restored[1].publication_id);
            assert_ne!(restored[0].publication_id, records[0].publication_id);
            assert_eq!(
                restored[0].published_at_wall_us,
                records[0].published_at_wall_us
            );
            assert!(scene.activate_notification(&second, 240).is_some());
        }
        assert_eq!(take_action_ids(&harness), ["second"]);
        assert!(harness.tick());
        {
            let mut scene = harness.scene();
            assert!(
                scene
                    .overlay
                    .zone_hit_regions
                    .iter()
                    .any(|r| r.publication_id == first.publication_id)
            );
            assert!(
                scene
                    .overlay
                    .zone_hit_regions
                    .iter()
                    .all(|r| r.publication_id != second.publication_id)
            );
            assert!(scene.hold_zone_publications("notification-area", "notif-agent", None));
            let records = scene.zone_registry.active_for_zone("notification-area");
            assert!(
                records[0].expires_at_wall_us.is_none(),
                "unanswered sibling can be held"
            );
            assert_eq!(
                records[1].expires_at_wall_us,
                Some(scene.now_wall_us() + 240_000)
            );
            // Stack eviction admits a same-time replacement while the chosen
            // exit still exists. It must not inherit the previous answer.
            scene
                .zone_registry
                .zones
                .get_mut("notification-area")
                .unwrap()
                .contention_policy = tze_hud_scene::ContentionPolicy::Stack { max_depth: 1 };
            scene
                .publish_to_zone(
                    "notification-area",
                    second_content,
                    "notif-agent",
                    None,
                    None,
                    None,
                )
                .unwrap();
            assert!(scene.overlay.notification_choices.is_empty());
            assert!(
                !scene
                    .overlay
                    .chrome_states
                    .contains_key(&second.interaction_id)
            );
            assert!(
                scene.activate_notification(&second, 240).is_none(),
                "stale answer cannot reach replacement"
            );
            assert!(scene.hold_zone_publications("notification-area", "notif-agent", None));
            let replacement = &scene.zone_registry.active_for_zone("notification-area")[0];
            assert_eq!(replacement.published_at_wall_us, first.published_at_wall_us);
            assert_ne!(replacement.publication_id, second.publication_id);
            assert!(replacement.expires_at_wall_us.is_none());
            // The forced trim tells the publisher its card was displaced.
            let receipts = scene.take_pending_actions("notif-agent");
            assert!(
                !receipts.is_empty()
                    && receipts
                        .iter()
                        .all(|a| a.kind == tze_hud_scene::graph::PendingActionKind::Displaced),
                "{receipts:?}"
            );
        }
        assert!(harness.tick());
        let (x, y) = harness.notification_action_center("second").unwrap();
        harness.click(x, y);
        harness.drain();
        assert_eq!(take_action_ids(&harness), ["second"]);
        assert_eq!(harness.zone_publication_count("notification-area"), 0);

        for dismiss in [false, true] {
            let mut harness = approve_reject_notification_harness();
            let content = harness
                .scene()
                .zone_registry
                .active_for_zone("notification-area")[0]
                .content
                .clone();
            {
                let mut scene = harness.scene();
                scene.clear_zone("notification-area").unwrap();
                scene
                    .publish_to_zone(
                        "notification-area",
                        content,
                        "notif-agent",
                        Some("slot".into()),
                        None,
                        None,
                    )
                    .unwrap();
            }
            assert!(harness.tick());
            let point = if dismiss {
                dismiss_center(&harness)
            } else {
                harness.notification_action_center("approve").unwrap()
            };
            harness.click(point.0, point.1);
            harness.drain();
            let accepted = harness.scene().take_pending_actions("notif-agent");
            assert_eq!(accepted.len(), 1);
            assert_eq!(accepted[0].key.as_deref(), Some("slot"));
            assert_eq!(
                accepted[0].kind,
                if dismiss {
                    tze_hud_scene::graph::PendingActionKind::Dismiss
                } else {
                    tze_hud_scene::graph::PendingActionKind::Callback("approve".into())
                }
            );
        }
    }

    /// A release activates an action only when the press began on that same
    /// button: pressing Approve and releasing over Reject, or pressing the
    /// dismiss button, queues no action the user did not press.
    #[test]
    fn release_activates_only_the_button_the_press_began_on() {
        let mut harness = approve_reject_notification_harness();
        let approve = harness.notification_action_center("approve").unwrap();
        let reject = harness.notification_action_center("reject").unwrap();

        harness.app.state.cursor_x = approve.0;
        harness.app.state.cursor_y = approve.1;
        harness.app.enqueue_pointer_event(PointerEventKind::Down);
        harness.app.state.cursor_x = reject.0;
        harness.app.state.cursor_y = reject.1;
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        // An Up with no press at all activates nothing either.
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        harness.drain();
        assert!(
            take_action_ids(&harness).is_empty(),
            "a press that began on Approve must not fire Reject"
        );

        let (x, y) = dismiss_center(&harness);
        // A mismatched release cannot turn either button into a human X.
        harness.app.state.cursor_x = x;
        harness.app.state.cursor_y = y;
        harness.app.enqueue_pointer_event(PointerEventKind::Down);
        harness.app.state.cursor_x = approve.0;
        harness.app.state.cursor_y = approve.1;
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        harness.drain();
        assert!(take_action_ids(&harness).is_empty());
        assert_eq!(harness.zone_publication_count("notification-area"), 1);

        harness.click(x, y);
        harness.drain();
        {
            let shared = harness.app.state.shared_state.blocking_lock();
            let mut scene = shared.scene.blocking_lock();
            let events = scene.take_pending_actions("notif-agent");
            assert_eq!(events.len(), 1, "one validated removal queues one human X");
            assert_eq!(events[0].publisher_namespace, "notif-agent");
            assert_eq!(events[0].zone_name, "notification-area");
            assert_eq!(
                events[0].kind,
                tze_hud_scene::graph::PendingActionKind::Dismiss
            );
            assert!(scene.take_pending_actions("another-agent").is_empty());
        }
        assert_eq!(harness.zone_publication_count("notification-area"), 0);
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        harness.click(x, y);
        harness.drain();
        assert!(
            take_action_ids(&harness).is_empty(),
            "late/stale releases create no second event"
        );
        for terminal in ["clear", "expiry", "replace"] {
            let mut harness = approve_reject_notification_harness();
            let approve = harness.notification_action_center("approve").unwrap();
            harness.app.state.cursor_x = approve.0;
            harness.app.state.cursor_y = approve.1;
            harness.app.enqueue_pointer_event(PointerEventKind::Down);
            {
                let mut scene = harness.scene();
                let old = scene.zone_registry.active_for_zone("notification-area")[0].clone();
                if terminal == "expiry" {
                    scene
                        .zone_registry
                        .active_publishes
                        .get_mut("notification-area")
                        .unwrap()[0]
                        .expires_at_wall_us = Some(0);
                    scene.drain_expired_zone_publications();
                } else {
                    scene.clear_zone("notification-area").unwrap();
                    if terminal == "replace" {
                        scene
                            .publish_to_zone(
                                "notification-area",
                                old.content,
                                "notif-agent",
                                None,
                                None,
                                None,
                            )
                            .unwrap();
                    }
                }
            }
            harness.drain();
            harness.app.enqueue_pointer_event(PointerEventKind::Up);
            harness.drain();
            assert!(
                take_action_ids(&harness).is_empty(),
                "{terminal} between Down/Up cannot answer"
            );
        }

        for terminal in ["replace", "clear", "expiry"] {
            for dismiss in [false, true] {
                let mut harness = approve_reject_notification_harness();
                let content = harness
                    .scene()
                    .zone_registry
                    .active_for_zone("notification-area")[0]
                    .content
                    .clone();
                {
                    let mut scene = harness.scene();
                    scene.clear_zone("notification-area").unwrap();
                    scene
                        .publish_to_zone(
                            "notification-area",
                            content.clone(),
                            "notif-agent",
                            Some("slot".into()),
                            None,
                            None,
                        )
                        .unwrap();
                }
                assert!(harness.tick());
                let point = if dismiss {
                    dismiss_center(&harness)
                } else {
                    harness.notification_action_center("approve").unwrap()
                };
                harness.app.state.cursor_x = point.0;
                harness.app.state.cursor_y = point.1;
                harness.app.enqueue_pointer_event(PointerEventKind::Down);
                harness.drain();
                {
                    let mut scene = harness.scene();
                    if terminal == "replace" {
                        scene
                            .publish_to_zone(
                                "notification-area",
                                content,
                                "notif-agent",
                                Some("slot".into()),
                                None,
                                None,
                            )
                            .unwrap();
                    } else if terminal == "clear" {
                        scene
                            .clear_notification_key("notification-area", "notif-agent", "slot")
                            .unwrap();
                    } else {
                        scene
                            .zone_registry
                            .active_publishes
                            .get_mut("notification-area")
                            .unwrap()[0]
                            .expires_at_wall_us = Some(0);
                        scene.drain_expired_zone_publications();
                    }
                }
                assert!(harness.tick());
                harness.app.enqueue_pointer_event(PointerEventKind::Up);
                harness.drain();
                assert!(
                    harness
                        .scene()
                        .take_pending_actions("notif-agent")
                        .is_empty(),
                    "{terminal} retired the original exact control"
                );
            }
        }
        for terminal in [
            "move-off",
            "focus",
            "expiry",
            "clear",
            "replacement",
            "hidden",
            "dismiss",
        ] {
            let mut harness = approve_reject_notification_harness();
            {
                let mut scene = harness.scene();
                let tze_hud_scene::ZoneContent::Notification(n) = &mut scene
                    .zone_registry
                    .active_publishes
                    .get_mut("notification-area")
                    .unwrap()[0]
                    .content
                else {
                    unreachable!()
                };
                n.actions[0].callback_id = None;
                n.actions[0].url = Some("https://example.com/stale".into());
            }
            assert!(harness.tick());
            harness.app.state.notification_link_opener.worker =
                Some(Arc::new(|_| panic!("retired link must not open")));
            let point = harness
                .notification_link_center("https://example.com/stale")
                .unwrap();
            let old = harness
                .scene()
                .zone_registry
                .active_for_zone("notification-area")[0]
                .clone();
            let window = winit::window::WindowId::dummy();
            let native = super::super::lifecycle::ChromeCaptureTestScope::new(window);
            (harness.app.state.cursor_x, harness.app.state.cursor_y) = point;
            let receipt = native.begin(true);
            harness
                .app
                .handle_mouse_button(winit::event::MouseButton::Left, true, window, None);
            native.complete(receipt);
            match terminal {
                "move-off" => {
                    harness.app.state.cursor_x = 0.0;
                    harness.app.state.cursor_y = 0.0;
                }
                "focus" => harness.app.handle_window_focus(None, false),
                "expiry" => {
                    harness
                        .scene()
                        .zone_registry
                        .active_publishes
                        .get_mut("notification-area")
                        .unwrap()[0]
                        .expires_at_wall_us = Some(0);
                    harness.scene().drain_expired_zone_publications();
                }
                "clear" | "replacement" => {
                    harness.scene().clear_zone("notification-area").unwrap();
                    if terminal == "replacement" {
                        harness
                            .scene()
                            .publish_to_zone(
                                "notification-area",
                                old.content,
                                "notif-agent",
                                None,
                                None,
                                None,
                            )
                            .unwrap();
                        assert!(harness.tick());
                    }
                }
                "hidden" => {
                    harness.scene().set_owner_hidden(
                        tze_hud_scene::graph::visibility::SurfaceClasses {
                            zones: true,
                            ..Default::default()
                        },
                    );
                }
                "dismiss" => {
                    assert!(harness.scene().dismiss_notification(
                        "notification-area",
                        old.publication_id,
                        "notif-agent"
                    ));
                }
                _ => unreachable!(),
            }
            let receipt = native.begin(false);
            harness
                .app
                .handle_mouse_button(winit::event::MouseButton::Left, false, window, None);
            native.complete(receipt);
            harness.drain();
            assert!(
                harness
                    .app
                    .state
                    .notification_link_opener
                    .workers
                    .is_empty(),
                "{terminal}"
            );
            assert!(take_action_ids(&harness).is_empty(), "{terminal}");
        }
    }

    /// A pointer press on a notification action button, dispatched through the
    /// real `lifecycle.rs` pointer-up path, queues the action for `hud_input`
    /// and dismisses the answered notification.
    #[test]
    fn pointer_up_on_action_enqueues_pending_action() {
        use tze_hud_scene::{
            ContentionPolicy, GeometryPolicy, LayerAttachment, NotificationAction,
            NotificationPayload, RenderingPolicy, ZoneContent, ZoneDefinition, ZoneMediaType,
        };

        let mut harness = HeadlessEventLoopHarness::new();
        {
            let shared = harness.app.state.shared_state.blocking_lock();
            let mut scene = shared.scene.blocking_lock();
            *scene = SceneGraph::new(1920.0, 1080.0);
            scene.create_tab("Main", 0).unwrap();
            scene.register_zone(ZoneDefinition {
                id: SceneId::new(),
                name: "notification-area".to_string(),
                description: "notifications".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.6,
                    y_pct: 0.0,
                    width_pct: 0.35,
                    height_pct: 0.3,
                },
                accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::Stack { max_depth: 5 },
                max_publishers: 4,
                auto_clear_ms: None,
                layer_attachment: LayerAttachment::Chrome,
                ephemeral: false,
            });
            scene
                .publish_to_zone(
                    "notification-area",
                    ZoneContent::Notification(NotificationPayload {
                        progress: None,
                        text: "Ship v2?".to_string(),
                        icon: String::new(),
                        urgency: 1,
                        ttl_ms: None,
                        title: "Deploy".to_string(),
                        actions: vec![NotificationAction {
                            copy: None,
                            reply: None,
                            label: "Ship".to_string(),
                            callback_id: Some("ship".to_string()),
                            url: None,
                            role: None,
                        }],
                    }),
                    "notif-agent",
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
        harness
            .app
            .state
            .global_tokens
            .insert("motion.exit.ms".into(), "0".into());
        while !harness.tick() {}

        let (x, y) = harness
            .notification_action_center("ship")
            .expect("the Ship button has a hit region");
        harness.click(x, y);
        harness.drain();

        let shared = harness.app.state.shared_state.blocking_lock();
        let mut scene = shared.scene.blocking_lock();
        let actions = scene.take_pending_actions("notif-agent");
        assert_eq!(actions.len(), 1, "one press queues one action");
        assert_eq!(
            actions[0].kind,
            tze_hud_scene::graph::PendingActionKind::Callback("ship".into())
        );
        assert_eq!(actions[0].zone_name, "notification-area");
        assert_eq!(
            scene
                .zone_registry
                .active_for_zone("notification-area")
                .len(),
            0,
            "an action press dismisses the answered notification"
        );

        drop(scene);
        drop(shared);

        use tze_hud_input::ScrollEvent;
        use tze_hud_scene::{TileScrollConfig, types::ZoneHitRegion};

        // Use real publications and the harness's compositor hit-region pass.
        let shared_state = Arc::clone(&harness.app.state.shared_state);
        {
            let shared = shared_state.blocking_lock();
            let mut scene = shared.scene.blocking_lock();
            scene
                .publish_to_zone(
                    "notification-area",
                    ZoneContent::Notification(NotificationPayload {
                        progress: None,
                        text: "Dismiss this notification".into(),
                        icon: String::new(),
                        urgency: 1,
                        ttl_ms: None,
                        title: "Target".into(),
                        actions: Vec::new(),
                    }),
                    "notif-agent",
                    None,
                    None,
                    None,
                )
                .unwrap();
            scene
                .publish_to_zone(
                    "notification-area",
                    ZoneContent::Notification(NotificationPayload {
                        progress: None,
                        text: "Keep this notification".into(),
                        icon: String::new(),
                        urgency: 1,
                        ttl_ms: None,
                        title: "Control".into(),
                        actions: vec![NotificationAction {
                            copy: None,
                            reply: None,
                            label: "Keep".into(),
                            callback_id: Some("keep".into()),
                            url: None,
                            role: None,
                        }],
                    }),
                    "control-agent",
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
        while !harness.tick() {}
        let notifications = || {
            let shared = shared_state.blocking_lock();
            let scene = shared.scene.blocking_lock();
            (
                scene
                    .zone_registry
                    .active_for_zone("notification-area")
                    .to_vec(),
                scene.overlay.zone_hit_regions.clone(),
            )
        };
        let before_dismiss = notifications();
        assert_eq!(before_dismiss.0.len(), 2);
        let target = before_dismiss
            .0
            .iter()
            .find(|record| record.publisher_namespace == "notif-agent")
            .expect("the action notification remains the dismiss target");
        let dismiss = before_dismiss
            .1
            .iter()
            .find(|hit| {
                hit.zone_name == target.zone_name
                    && hit.publisher_namespace == target.publisher_namespace
                    && hit.published_at_wall_us == target.published_at_wall_us
                    && matches!(hit.kind, ZoneInteractionKind::Dismiss)
            })
            .expect("the real notification pass generated the target dismiss hit");
        harness.app.state.cursor_x = dismiss.bounds.x + dismiss.bounds.width / 2.0;
        harness.app.state.cursor_y = dismiss.bounds.y + dismiss.bounds.height / 2.0;
        harness.app.enqueue_pointer_event(PointerEventKind::Down);
        assert_eq!(
            notifications(),
            before_dismiss,
            "Down cannot dismiss either identity"
        );
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        let mut after_dismiss = before_dismiss.clone();
        after_dismiss.0.retain(|record| {
            record.publisher_namespace != target.publisher_namespace
                || record.published_at_wall_us != target.published_at_wall_us
        });
        after_dismiss.1.retain(|hit| {
            hit.zone_name != target.zone_name
                || hit.publisher_namespace != target.publisher_namespace
                || hit.published_at_wall_us != target.published_at_wall_us
        });
        assert_eq!(after_dismiss.0.len(), 1);
        assert_eq!(after_dismiss.0[0].publisher_namespace, "control-agent");
        assert!(
            after_dismiss
                .1
                .iter()
                .any(|hit| { matches!(&hit.kind, ZoneInteractionKind::Action { index: 0 }) }),
            "the unrelated action hit survives"
        );
        assert_eq!(
            notifications(),
            after_dismiss,
            "Up removes only the exact publication and its stale hits"
        );
        // Do not repopulate cards between these events: this Up has no target hit.
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        assert_eq!(
            notifications(),
            after_dismiss,
            "another Up cannot dismiss the control"
        );

        let (tile_id, control_id, untracked_id, tail_offset) = {
            let shared = shared_state.blocking_lock();
            let mut scene = shared.scene.blocking_lock();
            let tab = scene.active_tab.unwrap();
            let lease = scene.grant_lease("scroll-agent", 60_000);
            let tile = scene
                .create_tile(
                    tab,
                    "scroll-agent",
                    lease,
                    Rect::new(100.0, 400.0, 400.0, 100.0),
                    1,
                )
                .unwrap();
            let control = scene
                .create_tile(
                    tab,
                    "scroll-agent",
                    lease,
                    Rect::new(700.0, 400.0, 400.0, 100.0),
                    1,
                )
                .unwrap();
            let untracked = scene
                .create_tile(
                    tab,
                    "scroll-agent",
                    lease,
                    Rect::new(100.0, 700.0, 400.0, 100.0),
                    1,
                )
                .unwrap();
            for id in [tile, control, untracked] {
                scene
                    .register_tile_scroll_config(id, TileScrollConfig::vertical())
                    .unwrap();
            }
            let processor = &mut harness.app.state.input_processor;
            processor.notify_tile_content_appended(tile, 400.0, 100.0, 20.0, &mut scene);
            let tail = scene.tile_scroll_offset_local(tile);
            assert_eq!(
                tail,
                (0.0, 300.0),
                "content minus viewport independently pins the tail"
            );
            processor
                .process_scroll_event(
                    &ScrollEvent {
                        x: 200.0,
                        y: 450.0,
                        delta_x: 0.0,
                        delta_y: -120.0,
                    },
                    &mut scene,
                )
                .expect("a real scroll event moves its tracked tile");
            assert_ne!(scene.tile_scroll_offset_local(tile), tail);
            assert!(!scene.tile_follow_tail_at_tail(tile));
            processor.notify_tile_content_appended(control, 600.0, 100.0, 20.0, &mut scene);
            processor
                .process_scroll_event(
                    &ScrollEvent {
                        x: 800.0,
                        y: 450.0,
                        delta_x: 0.0,
                        delta_y: -100.0,
                    },
                    &mut scene,
                )
                .expect("a real scroll event moves its tracked tile");
            assert!(!scene.tile_follow_tail_at_tail(control));
            // Existing scene state with no InputProcessor registration must stay intact.
            assert_eq!(processor.tile_total_content_height_px(untracked), 0.0);
            scene
                .set_tile_scroll_offset_local(untracked, 0.0, 37.0)
                .unwrap();
            scene.set_tile_follow_tail_at_tail(untracked, false);
            // Typed input fixture only; this does not assert compositor button geometry.
            scene.overlay.zone_hit_regions.push(ZoneHitRegion {
                publication_id: tze_hud_scene::SceneId::null(),
                zone_name: "__jump_to_latest__".into(),
                published_at_wall_us: 0,
                publisher_namespace: "runtime".into(),
                bounds: Rect::new(550.0, 550.0, 20.0, 20.0),
                kind: ZoneInteractionKind::JumpToLatest { tile_id: tile },
                interaction_id: format!("jump-to-latest:{tile}"),
                tab_order: 0,
            });
            let version = scene.version;
            scene.prune_notification_feedback();
            assert_eq!(scene.version, version, "cleanup does not request a redraw");
            (tile, control, untracked, tail)
        };
        let target_scroll = || {
            let shared = shared_state.blocking_lock();
            let scene = shared.scene.blocking_lock();
            (
                scene.tile_scroll_offset_local(tile_id),
                scene.tile_follow_tail_at_tail(tile_id),
            )
        };
        let unrelated = || {
            let shared = shared_state.blocking_lock();
            let scene = shared.scene.blocking_lock();
            (
                scene.tiles.clone(),
                scene.leases.clone(),
                scene.tile_scroll_offset_local(control_id),
                scene.tile_follow_tail_at_tail(control_id),
                scene.tile_scroll_offset_local(untracked_id),
                scene.tile_follow_tail_at_tail(untracked_id),
                scene
                    .zone_registry
                    .active_for_zone("notification-area")
                    .to_vec(),
                scene.overlay.zone_hit_regions.clone(),
            )
        };
        let scrolled_back = target_scroll();
        let control_before_jump = unrelated();
        harness.app.state.cursor_x = 560.0;
        harness.app.state.cursor_y = 560.0;
        harness.app.enqueue_pointer_event(PointerEventKind::Down);
        assert_eq!(target_scroll(), scrolled_back, "Down cannot jump");
        assert_eq!(unrelated(), control_before_jump);
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        assert_eq!(
            target_scroll(),
            (tail_offset, true),
            "Up restores the recorded tail and AtTail synchronously"
        );
        assert_eq!(
            unrelated(),
            control_before_jump,
            "other tiles, leases and publications are unchanged"
        );
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        assert_eq!(
            target_scroll(),
            (tail_offset, true),
            "Up at tail is idempotent"
        );
        assert_eq!(unrelated(), control_before_jump);
        {
            let shared = shared_state.blocking_lock();
            let mut scene = shared.scene.blocking_lock();
            let jump = scene.overlay.zone_hit_regions.last_mut().unwrap();
            assert!(
                matches!(jump.kind, ZoneInteractionKind::JumpToLatest { tile_id: id } if id == tile_id)
            );
            jump.kind = ZoneInteractionKind::JumpToLatest {
                tile_id: untracked_id,
            };
            jump.interaction_id = format!("jump-to-latest:{untracked_id}");
        }
        let before_untracked_jump = unrelated();
        harness.app.enqueue_pointer_event(PointerEventKind::Down);
        assert_eq!(unrelated(), before_untracked_jump);
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        assert_eq!(
            unrelated(),
            before_untracked_jump,
            "an untracked scroll-state jump is a no-op"
        );
        assert_eq!(target_scroll(), (tail_offset, true));
    }

    /// Hovering a tile publishes it as the close-button target (and not while the
    /// pointer is outside the window); a press on the close button, dispatched
    /// through the real pointer-up path, removes the tile on the spot and
    /// revokes its internal lease.
    #[test]
    fn viewer_close_button_dismisses_hovered_tile() {
        use tze_hud_scene::types::ZoneHitRegion;

        let mut harness = HeadlessEventLoopHarness::new();
        let (lease_id, tile_id) = {
            let shared = harness.app.state.shared_state.blocking_lock();
            let mut scene = shared.scene.blocking_lock();
            *scene = SceneGraph::new(1920.0, 1080.0);
            let tab = scene.create_tab("Main", 0).unwrap();
            let lease = scene.grant_lease("agent", 60_000);
            let tile = scene
                .create_tile(
                    tab,
                    "agent",
                    lease,
                    Rect::new(100.0, 100.0, 400.0, 300.0),
                    5,
                )
                .unwrap();
            harness.app.state.pipeline.hit_test_snapshot.store(Arc::new(
                crate::pipeline::HitTestSnapshot::from_scene(&scene),
            ));
            (lease, tile)
        };

        let hover = |app: &mut WinitApp, x: f32, y: f32| {
            app.state.cursor_x = x;
            app.state.cursor_y = y;
            app.push_tile_close_hover();
            *app.state.tile_close_hover_state.lock().unwrap()
        };
        assert_eq!(hover(&mut harness.app, 300.0, 200.0), Some(tile_id));
        assert_eq!(hover(&mut harness.app, 900.0, 900.0), None);
        harness.app.state.cursor_left_window = true;
        assert_eq!(hover(&mut harness.app, 300.0, 200.0), None);
        // An OS cursor read (the click-through path) overrides a stale
        // CursorLeft from the last capture flip.
        harness.app.apply_os_cursor_position(300.0, 200.0);
        harness.app.push_tile_close_hover();
        assert_eq!(
            *harness.app.state.tile_close_hover_state.lock().unwrap(),
            Some(tile_id)
        );

        // The compositor registers the button's hit region once the tile is
        // hovered; stand in for it here (the harness has no compositor).
        let (bx, by) = (450.0, 110.0);
        harness
            .app
            .state
            .shared_state
            .blocking_lock()
            .scene
            .blocking_lock()
            .overlay
            .zone_hit_regions
            .push(ZoneHitRegion {
                publication_id: tze_hud_scene::SceneId::null(),
                zone_name: "__chrome_tile_close__".into(),
                published_at_wall_us: 0,
                publisher_namespace: "runtime".into(),
                bounds: Rect::new(bx - 10.0, by - 10.0, 20.0, 20.0),
                kind: ZoneInteractionKind::DismissTile { tile_id },
                interaction_id: format!("tile-close:{tile_id}"),
                tab_order: 0,
            });
        {
            let mut scene = harness.scene();
            let version = scene.version;
            scene.prune_notification_feedback();
            assert_eq!(scene.version, version, "cleanup does not request a redraw");
        }
        harness.click(bx, by);
        harness.drain();

        assert_eq!(harness.tile_count(), 0, "dismiss is local and immediate");
        let shared = harness.app.state.shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        assert_eq!(
            scene.leases.get(&lease_id).unwrap().state,
            tze_hud_scene::types::LeaseState::Revoked
        );
        drop(scene);
        drop(shared);

        // Exercise the same real pointer path on a driver-bound portal, with
        // its overlapping header band and a separate live portal left alone.
        use tze_hud_portal::hub::{PortalKey, Publish};
        let portals = harness.app.state.portal_projection_driver.portal_handle();
        let key = PortalKey {
            agent: "close-agent".into(),
            id: "closing".into(),
        };
        let other = PortalKey {
            agent: "close-agent".into(),
            id: "remaining".into(),
        };
        for key in [&key, &other] {
            portals
                .hub
                .lock()
                .unwrap()
                .publish(
                    key,
                    Publish {
                        text: Some("portal output".into()),
                        ..Default::default()
                    },
                    portals.clock.now_us(),
                )
                .unwrap();
        }
        harness.app.drain_portal_projection();
        portals
            .submit_reply(&other, "keep this input".into(), portals.clock.now_us())
            .unwrap()
            .result
            .unwrap();
        let (closing_tile, closing_lease, remaining_tile) = {
            let mut scene = harness.scene();
            let find = |id: &str| {
                scene
                    .tiles
                    .values()
                    .find(|tile| {
                        scene
                            .portal_surface(tile.id)
                            .is_some_and(|surface| surface.identity.session_id.ends_with(id))
                    })
                    .unwrap()
                    .id
            };
            let tile = find("closing");
            let remaining = find("remaining");
            let lease = scene.tiles[&tile].lease_id;
            let bounds = scene.tiles[&tile].bounds;
            let band = scene
                .portal_header_band_anchors(40.0)
                .into_iter()
                .find(|(id, _)| *id == tile)
                .unwrap()
                .1;
            scene
                .overlay
                .drag_handle_hit_regions
                .push(tze_hud_scene::DragHandleHitRegion {
                    element_id: tile,
                    element_kind: tze_hud_scene::DragHandleElementKind::Tile,
                    bounds: band,
                    element_bounds: None,
                    interaction_id: "portal-header".into(),
                    hit_region: HitRegionNode {
                        accepts_pointer: true,
                        bounds: band,
                        ..Default::default()
                    },
                    tab_order: 0,
                    is_header_band: true,
                    resize_grip: None,
                });
            scene.overlay.zone_hit_regions.clear();
            scene.overlay.zone_hit_regions.push(ZoneHitRegion {
                publication_id: SceneId::null(),
                zone_name: "__chrome_tile_close__".into(),
                published_at_wall_us: 0,
                publisher_namespace: "runtime".into(),
                bounds: Rect::new(bounds.x + bounds.width - 28.0, bounds.y + 6.0, 22.0, 22.0),
                kind: ZoneInteractionKind::DismissTile { tile_id: tile },
                interaction_id: "portal-close".into(),
                tab_order: 0,
            });
            (tile, lease, remaining)
        };
        let close = harness.scene().overlay.zone_hit_regions[0].bounds;
        // Releasing on the close after a press elsewhere must not dismiss.
        harness.app.state.cursor_x = close.x - 100.0;
        harness.app.state.cursor_y = close.y;
        harness.app.enqueue_pointer_event(PointerEventKind::Down);
        harness.app.state.cursor_x = close.x + 10.0;
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        harness.drain();
        assert!(harness.scene().tiles.contains_key(&closing_tile));
        harness.app.state.cursor_x = close.x + 10.0;
        harness.app.enqueue_pointer_event(PointerEventKind::Down);
        harness.app.state.cursor_x = close.x - 100.0;
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        harness.drain();
        assert!(harness.scene().tiles.contains_key(&closing_tile));
        harness.click(close.x + 10.0, close.y + 10.0);
        harness.drain();
        assert!(!harness.scene().tiles.contains_key(&closing_tile));
        assert!(harness.scene().tiles.contains_key(&remaining_tile));
        assert_eq!(
            harness.scene().leases[&closing_lease].state,
            tze_hud_scene::types::LeaseState::Revoked
        );
        assert!(
            portals.hub.lock().unwrap().get(&key).is_none(),
            "actual click reclaims Hub attachment"
        );
        assert_eq!(
            portals
                .hub
                .lock()
                .unwrap()
                .get(&other)
                .unwrap()
                .inputs
                .len(),
            1
        );
        portals
            .hub
            .lock()
            .unwrap()
            .publish(
                &key,
                Publish {
                    text: Some("reattached output".into()),
                    ..Default::default()
                },
                portals.clock.now_us(),
            )
            .unwrap();
        harness.app.drain_portal_projection();
        assert_eq!(
            harness.tile_count(),
            2,
            "publish reattaches alongside unaffected portal"
        );
        assert!(!harness.scene().tiles.contains_key(&closing_tile));
    }

    /// Install one ordinary focused button (not a portal composer/control) and
    /// return its ids plus a receiver for the runtime's real input-event channel.
    fn install_button(
        harness: &mut HeadlessEventLoopHarness,
        initially_focused: bool,
    ) -> (SceneId, SceneId) {
        // Populate the authoritative scene rather than replacing its injected clock.
        let scene_handle = Arc::clone(&harness.app.state.shared_state.blocking_lock().scene);
        let mut scene = scene_handle.blocking_lock();
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("command-agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "command-agent",
                lease_id,
                Rect::new(0.0, 0.0, 400.0, 200.0),
                1,
            )
            .unwrap();
        let node_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id: node_id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(10.0, 10.0, 160.0, 48.0),
                        interaction_id: "primary-action".to_string(),
                        accepts_focus: true,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                },
            )
            .unwrap();

        harness.app.state.focus_manager.add_tab(tab_id);
        if initially_focused {
            // Use the direct command-focus helper so no PointerDown pre-sets
            // `pressed`; ACTIVATE must be the operation that changes it.
            let _ = harness
                .app
                .state
                .focus_manager
                .focus_node_via_command(tab_id, tile_id, node_id, &scene);
        }

        drop(scene);
        *harness.app.state.active_tab_mirror.lock().unwrap() = Some(tab_id);

        (tile_id, node_id)
    }

    #[test]
    fn focus_cleared_when_lease_revoked() {
        use tze_hud_input::FocusOwner;
        use tze_hud_scene::{TestClock, types::LeaseState};

        #[derive(Clone, Copy, Debug)]
        enum Removal {
            Revoke,
            GraceExpiry,
            MissingFallback,
        }

        for removal in [
            Removal::Revoke,
            Removal::GraceExpiry,
            Removal::MissingFallback,
        ] {
            // Exercise both real try_lock boundaries with a fresh scene per case.
            for busy_scene in [false, true] {
                let clock = Arc::new(TestClock::new(1_000));
                let mut harness = HeadlessEventLoopHarness::new();
                let shared_handle = harness.shared_state();
                let scene_handle = Arc::clone(&shared_handle.blocking_lock().scene);
                *scene_handle.blocking_lock() =
                    SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
                let (tile_id, node_id) = install_button(&mut harness, true);
                let (tab_id, lease_id, other_tab, other_tile, other_lease, grace_ms) = {
                    let mut scene = scene_handle.blocking_lock();
                    assert_eq!(scene.now_millis(), clock.now_millis());
                    let tab_id = scene.tiles[&tile_id].tab_id;
                    let lease_id = scene.tiles[&tile_id].lease_id;
                    let lease = &scene.leases[&lease_id];
                    let grace_ms = lease.grace_period_ms;
                    assert!(lease.ttl_ms > grace_ms + 1);
                    assert_eq!(lease.granted_at_ms, clock.now_millis());
                    if matches!(removal, Removal::MissingFallback) {
                        let fallback_lease = scene.grant_lease("fallback-agent", 60_000);
                        let fallback_tile = scene
                            .create_tile(
                                tab_id,
                                "fallback-agent",
                                fallback_lease,
                                Rect::new(500.0, 0.0, 100.0, 100.0),
                                2,
                            )
                            .unwrap();
                        harness.app.state.focus_manager.on_click(
                            tab_id,
                            fallback_tile,
                            None,
                            &scene,
                        );
                        harness.app.state.focus_manager.on_click(
                            tab_id,
                            tile_id,
                            Some(node_id),
                            &scene,
                        );
                        scene.revoke_lease(fallback_lease).unwrap();
                        assert!(!scene.tiles.contains_key(&fallback_tile));
                    }
                    let other_tab = scene.create_tab("Unrelated", 1).unwrap();
                    let other_lease = scene.grant_lease("other-agent", 60_000);
                    let other_tile = scene
                        .create_tile(
                            other_tab,
                            "other-agent",
                            other_lease,
                            Rect::new(0.0, 300.0, 100.0, 100.0),
                            1,
                        )
                        .unwrap();
                    harness.app.state.focus_manager.add_tab(other_tab);
                    harness
                        .app
                        .state
                        .focus_manager
                        .on_click(other_tab, other_tile, None, &scene);
                    (
                        tab_id,
                        lease_id,
                        other_tab,
                        other_tile,
                        other_lease,
                        grace_ms,
                    )
                };
                let owner = FocusOwner::Node { tile_id, node_id };
                let ring = Some(tze_hud_compositor::FocusRingOwner {
                    tab_id,
                    tile_id,
                    node_id: Some(node_id),
                });
                assert!(
                    harness
                        .app
                        .state
                        .focus_manager
                        .current_owner(tab_id)
                        .tile_id()
                        .is_some()
                );
                assert!(harness.tick());
                let assert_retained_focus = |harness: &HeadlessEventLoopHarness| {
                    assert_eq!(
                        *harness.app.state.focus_manager.current_owner(tab_id),
                        owner
                    );
                    assert_eq!(
                        *harness.app.state.focus_ring_owner_state.lock().unwrap(),
                        ring
                    );
                };
                assert_retained_focus(&harness);

                if matches!(removal, Removal::GraceExpiry) {
                    scene_handle
                        .blocking_lock()
                        .disconnect_lease(&lease_id, clock.now_millis())
                        .unwrap();
                    clock.advance(grace_ms - 1);
                    for _ in 0..2 {
                        assert!(harness.tick());
                        let scene = scene_handle.blocking_lock();
                        assert_eq!(scene.now_millis(), clock.now_millis());
                        assert_eq!(scene.tiles[&tile_id].lease_id, lease_id);
                        assert!(scene.nodes.contains_key(&node_id));
                        assert_eq!(scene.leases[&lease_id].state, LeaseState::Orphaned);
                        assert_retained_focus(&harness);
                    }
                    clock.advance(2);
                    // Settle sees the retained tile; the following Stage 4 sweep reaps it.
                    assert!(harness.tick());
                    assert_eq!(
                        scene_handle.blocking_lock().leases[&lease_id].state,
                        LeaseState::Expired
                    );
                    assert_retained_focus(&harness);
                } else {
                    scene_handle.blocking_lock().revoke_lease(lease_id).unwrap();
                    assert_eq!(
                        scene_handle.blocking_lock().leases[&lease_id].state,
                        LeaseState::Revoked
                    );
                    assert!(scene_handle.blocking_lock().revoke_lease(lease_id).is_err());
                }
                {
                    let scene = scene_handle.blocking_lock();
                    assert!(!scene.tiles.contains_key(&tile_id), "{removal:?}");
                    assert!(!scene.nodes.contains_key(&node_id), "{removal:?}");
                }

                // Neither contention point may block the event loop or force cleanup.
                let shared_guard = (!busy_scene).then(|| shared_handle.blocking_lock());
                let scene_guard = busy_scene.then(|| scene_handle.blocking_lock());
                let main_before = harness.app.state.wake.main_work_generation();
                let compositor_before = harness.app.state.wake.compositor().checkpoint();
                assert!(!harness.tick());
                assert_retained_focus(&harness);
                drop(scene_guard);
                drop(shared_guard);
                // As in the real loop, retry after the availability wakes. The
                // waiters briefly acquire the released lock themselves.
                while if busy_scene {
                    harness.app.state.wake.compositor().checkpoint() == compositor_before
                } else {
                    harness.app.state.wake.main_work_generation() == main_before
                } {
                    std::thread::yield_now();
                }
                for _ in 0..3 {
                    assert!(harness.tick());
                    assert_eq!(
                        *harness.app.state.focus_manager.current_owner(tab_id),
                        FocusOwner::None
                    );
                    assert_eq!(
                        *harness.app.state.focus_ring_owner_state.lock().unwrap(),
                        None
                    );
                    let scene = scene_handle.blocking_lock();
                    assert!(!scene.tiles.contains_key(&tile_id));
                    assert!(!scene.nodes.contains_key(&node_id));
                    assert_eq!(scene.tiles[&other_tile].lease_id, other_lease);
                    assert_eq!(scene.leases[&other_lease].state, LeaseState::Active);
                    assert_eq!(scene.active_tab, Some(tab_id));
                    assert_eq!(
                        *harness.app.state.focus_manager.current_owner(other_tab),
                        FocusOwner::Tile(other_tile)
                    );
                }
            }
        }
    }

    /// Install two scrollable tiles with keyboard focus on the first while the
    /// cursor is over the second. This is the adversarial setup for keyboard
    /// scroll ownership: focus and pointer location deliberately disagree.
    fn install_conflicting_focus_and_pointer_scroll_tiles(
        harness: &mut HeadlessEventLoopHarness,
    ) -> (SceneId, SceneId) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("scroll-agent", 60_000);
        let focused_tile = scene
            .create_tile(
                tab_id,
                "scroll-agent",
                lease_id,
                Rect::new(0.0, 0.0, 400.0, 300.0),
                1,
            )
            .unwrap();
        let pointer_tile = scene
            .create_tile(
                tab_id,
                "scroll-agent",
                lease_id,
                Rect::new(500.0, 0.0, 400.0, 300.0),
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(focused_tile, tze_hud_scene::TileScrollConfig::vertical())
            .unwrap();
        scene
            .register_tile_scroll_config(pointer_tile, tze_hud_scene::TileScrollConfig::vertical())
            .unwrap();

        harness.app.state.focus_manager.add_tab(tab_id);
        let transition =
            harness
                .app
                .state
                .focus_manager
                .on_click(tab_id, focused_tile, None, &scene);
        assert!(
            transition.gained.is_some(),
            "test setup must focus first tile"
        );
        harness.app.state.cursor_x = 550.0;
        harness.app.state.cursor_y = 50.0;

        {
            let shared = harness.app.state.shared_state.blocking_lock();
            *shared.scene.blocking_lock() = scene;
        }
        *harness.app.state.active_tab_mirror.lock().unwrap() = Some(tab_id);

        (focused_tile, pointer_tile)
    }

    /// / system-shell "Tab Keyboard Shortcuts": the runtime
    /// consumes Ctrl+Tab before agent routing and changes the authoritative
    /// scene tab locally. Drive the real event-loop drain so this proves the
    /// production precedence seam, not only `shell::handle_shortcut` in
    /// isolation.
    #[test]
    fn ctrl_tab_switches_scene_and_key_down_never_reaches_agent() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, _node_id) = install_button(&mut harness, true);
        let second_tab = {
            let shared = harness.app.state.shared_state.blocking_lock();
            let mut scene = shared.scene.blocking_lock();
            scene.create_tab("Second", 1).unwrap()
        };
        harness.app.state.focus_manager.add_tab(second_tab);
        {
            let mut chrome = harness.app.state.chrome_state.write().unwrap();
            chrome.add_tab(10, "Configured One".to_string());
            chrome.add_tab(20, "Configured Two".to_string());
        }

        harness.enqueue(ctrl_key_down("Tab", "Tab", false, 1_000));
        harness.drain();

        let active_tab = {
            let shared = harness.app.state.shared_state.blocking_lock();
            let scene = shared.scene.blocking_lock();
            scene.active_tab
        };
        assert_eq!(
            active_tab,
            Some(second_tab),
            "Ctrl+Tab must execute the shell's next-tab action locally"
        );
        assert_eq!(
            *harness.app.state.active_tab_mirror.lock().unwrap(),
            Some(second_tab),
            "the shell tab switch must refresh the event-loop mirror"
        );
        {
            let chrome = harness.app.state.chrome_state.read().unwrap();
            assert_eq!(chrome.active_tab_index, 1);
            assert_eq!(chrome.tabs[0].name, "Configured One");
            assert_eq!(chrome.tabs[1].name, "Configured Two");
        }
    }

    /// Runtime tab topology can grow after chrome labels are configured. The
    /// shell reconciliation must retain labels for existing chrome slots and
    /// synthesize a neutral label only for the newly-discovered slot.
    #[test]
    fn ctrl_tab_preserves_configured_chrome_label_when_scene_adds_tab() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, _node_id) = install_button(&mut harness, true);
        let second_tab = {
            let shared = harness.app.state.shared_state.blocking_lock();
            let mut scene = shared.scene.blocking_lock();
            scene.create_tab("Second", 1).unwrap()
        };
        harness.app.state.focus_manager.add_tab(second_tab);
        {
            let mut chrome = harness.app.state.chrome_state.write().unwrap();
            chrome.add_tab(10, "Configured One".to_string());
        }

        harness.enqueue(ctrl_key_down("Tab", "Tab", false, 1_000));
        harness.drain();

        let chrome = harness.app.state.chrome_state.read().unwrap();
        assert_eq!(chrome.tabs[0].name, "Configured One");
        assert_eq!(chrome.tabs[1].name, "Tab 2");
    }

    /// A shell-owned KeyDown consumes its physical sequence. Its matching
    /// release must not surface as an impossible agent-visible KeyUp after the
    /// shell already intercepted the press.
    #[test]
    fn matching_shell_reserved_key_sequence_never_reaches_agent() {
        for (key_code, down_key, up_key, shift, repeat) in [
            ("Tab", "Tab", "Tab", false, false),
            ("KeyM", "m", "M", true, false),
            ("KeyM", "M", "m", true, false),
            ("KeyM", "M", "m", true, true),
        ] {
            for modifiers_held in [false, true] {
                let mut harness = HeadlessEventLoopHarness::new();
                let (_tile_id, _node_id) = install_button(&mut harness, true);
                let active_tab = *harness.app.state.active_tab_mirror.lock().unwrap();
                let chrome_tabs = harness.app.state.chrome_state.read().unwrap().tabs.len();

                harness.enqueue(ctrl_key_down(key_code, down_key, shift, 1_000));
                if repeat {
                    let mut repeated = ctrl_key_down(key_code, down_key, shift, 1_050);
                    if let PendingKeyboardEvent::KeyDown(raw) = &mut repeated {
                        raw.repeat = true;
                    }
                    harness.enqueue(repeated);
                }
                harness.drain();
                assert_eq!(harness.pending_len(), 0);

                if key_code == "KeyM" {
                    let shared = harness.app.state.shared_state.blocking_lock();
                    assert_eq!(shared.scene.blocking_lock().active_tab, active_tab);
                    assert_eq!(
                        *harness.app.state.active_tab_mirror.lock().unwrap(),
                        active_tab
                    );
                    assert_eq!(
                        harness.app.state.chrome_state.read().unwrap().tabs.len(),
                        chrome_tabs,
                        "Ctrl+Shift+M is a no-op, not a tab action"
                    );
                }

                harness.enqueue(PendingKeyboardEvent::KeyUp(RawKeyUpEvent {
                    key_code: key_code.to_string(),
                    key: up_key.to_string(),
                    // Physical identity owns the release even after Ctrl/Shift
                    // release changes both modifiers and the logical key case.
                    modifiers: if modifiers_held {
                        KeyboardModifiers {
                            ctrl: true,
                            shift,
                            ..KeyboardModifiers::NONE
                        }
                    } else {
                        KeyboardModifiers::NONE
                    },
                    timestamp_mono_us: MonoUs(1_100),
                }));
                harness.drain();
                assert_eq!(harness.pending_len(), 0);

                assert!(
                    harness
                        .app
                        .state
                        .consumed_shell_shortcut_keydowns
                        .is_empty()
                );
            }
        }
    }

    /// R4.1 S7: every Escape while a composer is active stays in
    /// the runtime, on a plain composer with no portal behind it: with text, a
    /// selection, an empty draft or a modifier, no Escape key or Cancel command
    /// reaches the tile's agent, and the draft text never changes.
    #[test]
    fn bare_escape_in_composer_never_reaches_agent() {
        let none = KeyboardModifiers::NONE;
        let shift = KeyboardModifiers {
            shift: true,
            ..none
        };
        let ctrl = KeyboardModifiers { ctrl: true, ..none };
        let alt = KeyboardModifiers { alt: true, ..none };
        for (draft, select, modifiers) in [
            ("typed", false, none),
            ("typed", true, none),
            ("", false, none),
            ("typed", false, shift),
            ("typed", false, ctrl),
            ("typed", false, alt),
        ] {
            let mut harness = HeadlessEventLoopHarness::new();
            harness.focus_composer();

            if !draft.is_empty() {
                harness.enqueue(character(draft, 100));
            }
            harness.drain();
            if select {
                harness
                    .app
                    .state
                    .input_processor
                    .route_pointer_selection_to_composer(0, 2);
            }
            for mut event in [
                key_down("Escape", "Escape", 200),
                key_up("Escape", "Escape", 300),
            ] {
                match &mut event {
                    PendingKeyboardEvent::KeyDown(raw) => raw.modifiers = modifiers,
                    PendingKeyboardEvent::KeyUp(raw) => raw.modifiers = modifiers,
                    _ => unreachable!(),
                }
                harness.enqueue(event);
            }
            harness.drain();
            let case = format!("{draft:?} select={select} {modifiers:?}");
            // Leaving reports an ordinary FocusLost; no key or command leaks.

            let texts: Vec<_> = harness
                .app
                .state
                .input_processor
                .composer_drafts()
                .map(|(_, d)| d.text().to_owned())
                .filter(|t| !t.is_empty())
                .collect();
            let expected: &[&str] = if draft.is_empty() { &[] } else { &[draft] };
            assert_eq!(texts, expected, "{case}: the text is unchanged");
            let modified = modifiers.ctrl || modifiers.alt;
            assert_eq!(
                harness.app.state.input_processor.is_composer_active(),
                select || modified,
                "{case}: a bare Esc without a selection leaves"
            );
        }
    }

    /// With no question, the answer chord's real
    /// main-thread handoff puts the caret in the newest waiting portal.
    #[test]
    fn answer_chord_with_no_card_focuses_the_newest_waiting_portal() {
        use tze_hud_portal::hub::{PortalKey, Publish};
        fn request_answer(harness: &mut HeadlessEventLoopHarness) {
            harness
                .app
                .state
                .answer_keyboard
                .requested
                .store(true, std::sync::atomic::Ordering::Release);
            harness.drain();
        }
        /// Publish `at` ms after the first publish, on the hub's own clock.
        fn publish(harness: &mut HeadlessEventLoopHarness, id: &str, ask: bool, at: u64) {
            static BASE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
            let portals = harness.app.state.portal_projection_driver.portal_handle();
            let at_us = BASE.get_or_init(|| portals.clock.now_us()) + at * 1_000;
            portals
                .hub
                .lock()
                .unwrap()
                .publish(
                    &PortalKey::new("ops", id),
                    Publish {
                        text: Some(format!("{id} at {at}")),
                        expects_reply: ask,
                        ..Default::default()
                    },
                    at_us,
                )
                .expect("publish accepted");
            harness.tick();
        }
        /// The portal whose composer has keyboard focus, by id.
        fn focused(harness: &HeadlessEventLoopHarness) -> Option<String> {
            let scene = harness.scene();
            let tab = scene.active_tab?;
            let tile = harness
                .app
                .state
                .focus_manager
                .current_owner(tab)
                .tile_id()?;
            harness
                .app
                .state
                .portal_projection_driver
                .key_for_tile(tile)
                .map(|key| key.id)
        }

        let mut harness = HeadlessEventLoopHarness::new();
        // S4: no card and no waiting portal changes nothing.
        publish(&mut harness, "a", false, 1);
        request_answer(&mut harness);
        assert_eq!(focused(&harness), None, "no portal is waiting");

        // S3: of two waiting portals, the newer request wins.
        publish(&mut harness, "a", true, 2);
        publish(&mut harness, "b", true, 3);
        request_answer(&mut harness);
        assert_eq!(focused(&harness).as_deref(), Some("b"));
        harness.enqueue(character("kept", 3_100));
        harness.drain();
        assert_eq!(harness.composer_draft().as_deref(), Some("kept"));
        request_answer(&mut harness);
        assert_eq!(focused(&harness).as_deref(), Some("b"), "again: a no-op");
        assert_eq!(harness.composer_draft().as_deref(), Some("kept"));

        // S1: a newer request elsewhere, then back; the draft is intact.
        publish(&mut harness, "a", true, 4);
        request_answer(&mut harness);
        assert_eq!(focused(&harness).as_deref(), Some("a"));
        publish(&mut harness, "b", true, 5);
        request_answer(&mut harness);
        assert_eq!(focused(&harness).as_deref(), Some("b"));
        assert_eq!(harness.composer_draft().as_deref(), Some("kept"));

        // S5: no agent receives anything from the focus move.
        {
            let portals = harness.app.state.portal_projection_driver.portal_handle();
            let hub = portals.hub.lock().unwrap();
            for id in ["a", "b"] {
                let portal = hub.get(&PortalKey::new("ops", id)).unwrap();
                assert!(portal.inputs.is_empty(), "{id}: no input queued");
            }
        }

        // S1: Enter in the chord-focused composer sends the draft exactly once,
        // and the composer keeps focus (a global chord is out of tier L's reach).
        harness.enqueue(key_down("Enter", "Enter", 5_100));
        harness.enqueue(key_up("Enter", "Enter", 5_200));
        harness.drain();
        {
            let portals = harness.app.state.portal_projection_driver.portal_handle();
            let hub = portals.hub.lock().unwrap();
            let texts = |id: &str| -> Vec<String> {
                let portal = hub.get(&PortalKey::new("ops", id)).unwrap();
                portal
                    .inputs
                    .iter()
                    .map(|input| input.text.clone())
                    .collect()
            };
            assert_eq!(texts("b"), ["kept"], "the reply is sent once");
            assert!(texts("a").is_empty(), "the other portal receives nothing");
        }
        assert_eq!(
            focused(&harness).as_deref(),
            Some("b"),
            "a sent reply keeps focus"
        );

        // S4: safe mode and hidden portals change nothing.
        let clear_focus = |harness: &mut HeadlessEventLoopHarness| {
            let tab = harness.scene().active_tab.unwrap();
            harness.app.state.focus_manager = tze_hud_input::FocusManager::new();
            harness.app.state.focus_manager.add_tab(tab);
        };
        clear_focus(&mut harness);
        harness
            .app
            .state
            .safe_mode_atomic
            .store(true, std::sync::atomic::Ordering::Release);
        request_answer(&mut harness);
        assert_eq!(focused(&harness), None, "paused");
        harness
            .app
            .state
            .safe_mode_atomic
            .store(false, std::sync::atomic::Ordering::Release);
        harness
            .scene()
            .set_owner_hidden(tze_hud_scene::graph::SurfaceClasses {
                portals: true,
                tiles: true,
                ..Default::default()
            });
        request_answer(&mut harness);
        assert_eq!(focused(&harness), None, "portals hidden");
        harness
            .scene()
            .set_owner_hidden(tze_hud_scene::graph::SurfaceClasses::default());

        // S4: a degraded portal is skipped; the other one still waits.
        let degrade = |harness: &mut HeadlessEventLoopHarness, id: &str| {
            let portals = harness.app.state.portal_projection_driver.portal_handle();
            let now = portals.clock.now_us();
            assert!(
                portals
                    .hub
                    .lock()
                    .unwrap()
                    .degrade(&PortalKey::new("ops", id), now)
            );
            harness.tick();
        };
        degrade(&mut harness, "b");
        request_answer(&mut harness);
        assert_eq!(focused(&harness).as_deref(), Some("a"), "b is degraded");
        clear_focus(&mut harness);
        degrade(&mut harness, "a");
        request_answer(&mut harness);
        assert_eq!(focused(&harness), None, "both degraded");
    }

    /// A question card wins over a waiting portal.
    #[test]
    fn answer_chord_prefers_a_question_card_over_a_waiting_portal() {
        let mut harness = approve_reject_notification_harness();
        let portals = harness.app.state.portal_projection_driver.portal_handle();
        portals
            .hub
            .lock()
            .unwrap()
            .publish(
                &tze_hud_portal::hub::PortalKey::new("ops", "ask"),
                tze_hud_portal::hub::Publish {
                    text: Some("Reply?".into()),
                    expects_reply: true,
                    ..Default::default()
                },
                portals.clock.now_us(),
            )
            .expect("publish accepted");
        assert!(harness.tick());
        harness
            .app
            .state
            .answer_keyboard
            .requested
            .store(true, std::sync::atomic::Ordering::Release);
        harness.drain();
        assert!(harness.app.state.answer_keyboard.focus.is_some());
        assert!(!harness.app.state.input_processor.is_composer_active());
    }

    /// The tray's answer goes through the panel task to the
    /// chord's main-thread path; a stale click opens the panel instead.
    #[test]
    fn tray_answer_focuses_the_question_or_opens_the_panel() {
        use crate::shell::control_panel::{PanelEvent, TrayCommand};
        use tze_hud_scene::config::ChordRole;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fixture =
            runtime.block_on(async { super::super::panel_input::panel_task_tests::fixture() });
        let panel = fixture.panel.clone();
        let wait = |what: &str, done: &dyn Fn() -> bool| {
            for _ in 0..400 {
                if done() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("{what}");
        };
        let tray = |harness: &mut HeadlessEventLoopHarness, command| {
            let requested = Arc::clone(&harness.app.state.answer_keyboard.tray_requested);
            assert!(panel.send(PanelEvent::Tray(command, Default::default())));
            if command == TrayCommand::AnswerNext {
                wait("the panel task ran the hook", &|| {
                    requested.lock().unwrap().is_some()
                });
                harness.drain();
            }
        };
        let attach = |harness: &mut HeadlessEventLoopHarness| {
            harness.app.state.control_panel = Some(panel.clone());
            panel.set_answer(
                harness
                    .app
                    .state
                    .answer_keyboard
                    .tray_hook(harness.app.state.wake.main_work_notifier()),
            );
        };

        // S1: an open panel closes and the question takes the keys.
        let mut harness = approve_reject_notification_harness();
        attach(&mut harness);
        panel.send(PanelEvent::Chord(ChordRole::Panel));
        wait("panel opens", &|| panel.is_open());
        harness.app.sync_panel_focus();
        tray(&mut harness, TrayCommand::AnswerNext);
        assert!(harness.app.state.answer_keyboard.focus.is_some());
        wait("panel closes", &|| !panel.is_open());
        harness.enqueue(key_down("Digit2", "2", 2_000));
        harness.enqueue(key_up("Digit2", "2", 2_100));
        harness.drain();
        assert_eq!(take_action_ids(&harness), ["reject"], "2 answers once");

        // S5: the question went before the click landed: the panel opens.
        tray(&mut harness, TrayCommand::AnswerNext);
        assert!(harness.app.state.answer_keyboard.focus.is_none());
        wait("a stale click opens the panel", &|| panel.is_open());
        tray(&mut harness, TrayCommand::AnswerNext);
        wait("and leaves it open", &|| panel.is_open());
        panel.send(PanelEvent::Chord(ChordRole::Panel));
        wait("panel closes", &|| !panel.is_open());

        // A paused HUD never takes card focus from the tray either.
        let mut harness = approve_reject_notification_harness();
        attach(&mut harness);
        harness
            .app
            .state
            .safe_mode_atomic
            .store(true, std::sync::atomic::Ordering::Release);
        tray(&mut harness, TrayCommand::AnswerNext);
        assert!(harness.app.state.answer_keyboard.focus.is_none());
        wait("paused: the panel opens", &|| panel.is_open());
        harness
            .app
            .state
            .safe_mode_atomic
            .store(false, std::sync::atomic::Ordering::Release);
        panel.send(PanelEvent::Chord(ChordRole::Panel));
        wait("panel closes", &|| !panel.is_open());

        // S4: with a question waiting, the panel stays reachable.
        tray(&mut harness, TrayCommand::OpenPanel);
        wait("Open panel opens it", &|| panel.is_open());
        assert!(harness.app.state.answer_keyboard.focus.is_none());
        panel.send(PanelEvent::Chord(ChordRole::Panel));
        wait("the panel chord closes it", &|| !panel.is_open());
        panel.send(PanelEvent::Chord(ChordRole::Panel));
        wait("and opens it", &|| panel.is_open());
        panel.send(PanelEvent::Chord(ChordRole::Panel));
        wait("panel closes", &|| !panel.is_open());
        panel.send(PanelEvent::Chord(ChordRole::Pause));
        wait("the pause chord opens it", &|| panel.is_open());
        panel.send(PanelEvent::Chord(ChordRole::Pause));
        wait("and resumes", &|| !panel.is_open());
        drop(fixture);
    }

    #[test]
    fn notification_keyboard_cycle_activation_and_return_preserve_portal_editor() {
        fn press(harness: &mut HeadlessEventLoopHarness, code: &str, key: &str, reverse: bool) {
            let mut down = key_down(code, key, 1_000);
            if let PendingKeyboardEvent::KeyDown(raw) = &mut down {
                raw.modifiers.shift = reverse;
            }
            harness.enqueue(down);
            harness.enqueue(key_up(code, key, 1_100));
            harness.drain();
        }
        fn request_answer(harness: &mut HeadlessEventLoopHarness) {
            harness
                .app
                .state
                .answer_keyboard
                .requested
                .store(true, std::sync::atomic::Ordering::Release);
            harness.drain();
        }
        fn card_with_editor() -> HeadlessEventLoopHarness {
            let mut harness = approve_reject_notification_harness();
            let (zone, content) = {
                let scene = harness.scene();
                (
                    scene.zone_registry.zones["notification-area"].clone(),
                    scene.zone_registry.active_for_zone("notification-area")[0]
                        .content
                        .clone(),
                )
            };
            harness.focus_composer();
            {
                let mut scene = harness.scene();
                scene.register_zone(zone);
                scene
                    .publish_to_zone(
                        "notification-area",
                        content,
                        "notif-agent",
                        Some("question".into()),
                        None,
                        None,
                    )
                    .unwrap();
            }
            assert!(harness.tick());
            harness
        }

        // Use the actual main-thread intent and pending-keyboard drain. Each
        // digit pins an ordinal, never the current label or another card.
        for (code, key, callback) in [
            ("Digit1", "1", "approve"),
            ("Digit2", "2", "reject"),
            ("Digit3", "3", "wait"),
            ("Digit4", "4", "details"),
        ] {
            let mut harness = card_with_editor();
            harness.enqueue(character("kept", 100));
            harness.drain();
            request_answer(&mut harness);
            let publication = harness
                .app
                .state
                .answer_keyboard
                .focus
                .unwrap()
                .publication_id;
            assert_eq!(
                harness.scene().overlay.notification_answer,
                Some(publication)
            );
            assert!(
                harness
                    .scene()
                    .overlay
                    .notification_focus
                    .as_deref()
                    .unwrap()
                    .ends_with(":0")
            );
            press(&mut harness, "ArrowLeft", "ArrowLeft", false);
            assert!(
                harness
                    .scene()
                    .overlay
                    .notification_focus
                    .as_deref()
                    .unwrap()
                    .ends_with(":3")
            );
            press(&mut harness, "Tab", "Tab", false);
            assert!(
                harness
                    .scene()
                    .overlay
                    .notification_focus
                    .as_deref()
                    .unwrap()
                    .ends_with(":0")
            );

            harness.enqueue(key_down(code, key, 200));
            let mut repeat = key_down(code, key, 201);
            if let PendingKeyboardEvent::KeyDown(raw) = &mut repeat {
                raw.repeat = true;
            }
            harness.enqueue(repeat);
            harness.enqueue(character(key, 202));
            harness.drain();
            assert_eq!(
                harness.app.state.answer_keyboard.return_key.as_deref(),
                Some(code),
                "return waits for the physical release"
            );
            assert!(harness.app.state.answer_keyboard.focus.is_none());
            assert_eq!(harness.composer_draft().as_deref(), Some("kept"));
            assert_eq!(take_action_ids(&harness), [callback]);
            harness.enqueue(key_up(code, key, 203));
            harness.drain();
            assert!(harness.app.state.answer_keyboard.return_key.is_none());

            assert!(
                take_action_ids(&harness).is_empty(),
                "repeat and release cannot answer twice"
            );
            assert!(harness.scene().overlay.notification_answer.is_none());
        }

        // A primary/secondary HWND blur is not a whole-HUD focus loss. The
        // production handler receives the current native foreground ownership.
        for secondary in [None, Some(0)] {
            for owned in [true, false] {
                let mut harness = card_with_editor();
                request_answer(&mut harness);
                let focused = harness.scene().overlay.notification_focus.clone();
                harness
                    .app
                    .handle_window_focus_with_foreground(secondary, false, owned);
                if owned {
                    assert!(harness.app.state.answer_keyboard.focus.is_some());
                    assert_eq!(harness.scene().overlay.notification_focus, focused);
                    assert!(
                        !harness
                            .app
                            .state
                            .input_processor
                            .chrome_feedback_cancel_pending
                    );
                    press(&mut harness, "Digit2", "2", false);
                    assert_eq!(take_action_ids(&harness), ["reject"]);
                } else {
                    assert!(harness.app.state.answer_keyboard.focus.is_none());
                    assert!(harness.app.state.answer_keyboard.return_key.is_none());
                    assert!(harness.tick());
                    assert!(harness.scene().overlay.notification_focus.is_none());
                    assert!(harness.scene().overlay.notification_answer.is_none());
                    assert!(take_action_ids(&harness).is_empty());
                }
            }
        }

        // Losing the whole HUD before an exit key is released cancels the
        // pending return. Its late release stays consumed without answering again.
        for (code, key, callbacks) in [
            ("Digit2", "2", vec!["reject"]),
            ("Escape", "Escape", vec![]),
        ] {
            let mut harness = card_with_editor();
            harness.enqueue(character("kept", 299));
            harness.drain();
            request_answer(&mut harness);
            harness.enqueue(key_down(code, key, 300));
            harness.drain();
            assert_eq!(take_action_ids(&harness), callbacks);
            assert_eq!(
                harness.app.state.answer_keyboard.return_key.as_deref(),
                Some(code)
            );
            harness
                .app
                .handle_window_focus_with_foreground(None, false, false);
            assert!(harness.app.state.answer_keyboard.return_key.is_none());
            harness.enqueue(key_up(code, key, 301));
            harness.drain();
            assert!(harness.app.state.answer_keyboard.return_key.is_none());
            assert!(take_action_ids(&harness).is_empty());
            assert_eq!(harness.composer_draft().as_deref(), Some("kept"));
        }

        // The global answer chord transfers input from an open running panel.
        // A request with no card and no waiting portal keeps its search and
        // panel completely intact.
        {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let fixture =
                runtime.block_on(async { super::super::panel_input::panel_task_tests::fixture() });
            let panel = fixture.panel.clone();
            let wait_open = |open| {
                runtime.block_on(async {
                    for _ in 0..200 {
                        if panel.is_open() == open {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    panic!("panel did not reach open={open}");
                })
            };
            use crate::shell::control_panel::PanelEvent;
            use tze_hud_scene::config::ChordRole;
            panel.send(PanelEvent::Chord(ChordRole::Panel));
            wait_open(true);
            let mut harness = card_with_editor();
            harness.app.state.control_panel = Some(panel.clone());
            harness.app.sync_panel_focus();
            request_answer(&mut harness);
            assert!(harness.app.state.answer_keyboard.focus.is_some());
            assert!(!harness.app.state.answer_keyboard.focus.unwrap().nested);
            let physical = winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Digit2);
            let logical = winit::keyboard::Key::Character("2".into());
            assert!(
                !harness
                    .app
                    .panel_key_input(None, physical, &logical, Some("2"), true)
            );
            assert!(!harness.app.panel_text(None, "2"));
            wait_open(false);
            harness.app.sync_panel_focus();
            assert!(harness.app.state.answer_keyboard.focus.is_some());
            assert!(harness.scene().overlay.notification_answer.is_some());
            press(&mut harness, "Digit2", "2", false);
            assert_eq!(take_action_ids(&harness), ["reject"]);
            assert!(harness.tick());
            panel.send(PanelEvent::Chord(ChordRole::Panel));
            wait_open(true);
            harness.app.sync_panel_focus();
            request_answer(&mut harness);
            assert!(harness.app.state.answer_keyboard.focus.is_none());
            assert!(
                panel.is_open(),
                "no card and no waiting portal must not close settings"
            );
            panel.send(PanelEvent::Chord(ChordRole::Panel));
            wait_open(false);
        }

        // An unavailable ordinal is inert; Enter/Space still use the moved
        // focus ring, and a safe-mode request cannot take card focus.
        for (activation, key) in [("Enter", "Enter"), ("Space", " ")] {
            let mut harness = approve_reject_notification_harness();
            {
                let mut scene = harness.scene();
                let tze_hud_scene::ZoneContent::Notification(n) = &mut scene
                    .zone_registry
                    .active_publishes
                    .get_mut("notification-area")
                    .unwrap()[0]
                    .content
                else {
                    unreachable!()
                };
                n.actions.truncate(2);
            }
            assert!(harness.tick());
            harness
                .app
                .state
                .safe_mode_atomic
                .store(true, std::sync::atomic::Ordering::Release);
            request_answer(&mut harness);
            assert!(harness.app.state.answer_keyboard.focus.is_none());
            harness
                .app
                .state
                .safe_mode_atomic
                .store(false, std::sync::atomic::Ordering::Release);
            request_answer(&mut harness);
            press(&mut harness, "Digit4", "4", false);
            assert!(take_action_ids(&harness).is_empty());
            assert!(harness.app.state.answer_keyboard.focus.is_some());
            press(&mut harness, "ArrowDown", "ArrowDown", false);
            press(&mut harness, activation, key, false);
            assert_eq!(take_action_ids(&harness), ["reject"]);
        }

        // Newest means the newest rendered unanswered callback card. A newer
        // link-only card is ineligible; no eligible card leaves existing focus
        // and geometry untouched, rather than flashing an empty visit.
        {
            let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
            let mut harness = approve_reject_notification_harness_on(clock.clone());
            let content = harness
                .scene()
                .zone_registry
                .active_for_zone("notification-area")[0]
                .content
                .clone();
            clock.advance(1);
            harness
                .scene()
                .publish_to_zone(
                    "notification-area",
                    content.clone(),
                    "notif-agent",
                    Some("new".into()),
                    None,
                    None,
                )
                .unwrap();
            let newest = harness
                .scene()
                .zone_registry
                .active_for_zone("notification-area")[1]
                .publication_id;
            clock.advance(1);
            let mut link_only = content;
            let tze_hud_scene::ZoneContent::Notification(n) = &mut link_only else {
                unreachable!()
            };
            for action in &mut n.actions {
                action.callback_id = None;
                action.url = Some("https://example.com/only".into());
            }
            harness
                .scene()
                .publish_to_zone(
                    "notification-area",
                    link_only,
                    "notif-agent",
                    Some("link".into()),
                    None,
                    None,
                )
                .unwrap();
            assert!(harness.tick());
            request_answer(&mut harness);
            assert_eq!(
                harness
                    .app
                    .state
                    .answer_keyboard
                    .focus
                    .unwrap()
                    .publication_id,
                newest
            );
            press(&mut harness, "Escape", "Escape", false);
            harness.scene().clear_zone("notification-area").unwrap();
            assert!(harness.tick());
            let before = harness.scene().geometry_epoch;
            request_answer(&mut harness);
            assert!(harness.app.state.answer_keyboard.focus.is_none());
            assert_eq!(harness.scene().geometry_epoch, before);
        }

        for terminal in ["replace", "expire"] {
            let mut harness = card_with_editor();
            harness.enqueue(character("kept", 100));
            harness.drain();
            request_answer(&mut harness);
            {
                let mut scene = harness.scene();
                if terminal == "replace" {
                    let content = scene.zone_registry.active_for_zone("notification-area")[0]
                        .content
                        .clone();
                    scene
                        .publish_to_zone(
                            "notification-area",
                            content,
                            "notif-agent",
                            Some("question".into()),
                            None,
                            None,
                        )
                        .unwrap();
                } else {
                    scene
                        .zone_registry
                        .active_publishes
                        .get_mut("notification-area")
                        .unwrap()[0]
                        .expires_at_wall_us = Some(0);
                    scene.drain_expired_zone_publications();
                }
            }
            assert!(harness.tick());
            press(&mut harness, "Digit2", "2", false);
            harness.enqueue(character("2", 200));
            harness.drain();
            assert!(
                take_action_ids(&harness).is_empty(),
                "a stale chord never retargets its replacement"
            );
            assert_eq!(harness.composer_draft().as_deref(), Some("kept"));
            assert!(harness.app.state.answer_keyboard.focus.is_some());
            press(&mut harness, "Escape", "Escape", false);
            assert!(harness.app.state.answer_keyboard.focus.is_none());
        }

        // Card and selection are separate inner layers; Esc never changes the
        // draft (R4.1 S1-S2): the next Esc leaves the editor with
        // its text kept, and refocusing restores text, caret and selection state.
        {
            let mut harness = card_with_editor();
            harness.enqueue(character("draft", 100));
            harness.drain();
            assert!(harness.app.focus_newest_question(true).unwrap());
            press(&mut harness, "Escape", "Escape", false);
            assert!(harness.app.state.input_processor.is_composer_active());
            assert_eq!(harness.composer_draft().as_deref(), Some("draft"));
            harness
                .app
                .state
                .input_processor
                .route_pointer_selection_to_composer(1, 3);
            press(&mut harness, "Escape", "Escape", false);
            let (text, cursor, anchor, ..) = harness
                .app
                .state
                .input_processor
                .composer_draft_snapshot()
                .unwrap();
            assert_eq!((text.as_str(), cursor, anchor), ("draft", 3, 3));
            harness.enqueue(key_down("Escape", "Escape", 300));
            harness.drain();
            assert!(!harness.app.state.input_processor.is_composer_active());
            assert_eq!(
                harness.app.state.answer_keyboard.return_key.as_deref(),
                Some("Escape"),
                "one Esc leaves a nonempty editor"
            );
            harness.enqueue(key_up("Escape", "Escape", 301));
            harness.drain();
            assert!(harness.app.state.answer_keyboard.return_key.is_none());
            let composer = harness
                .app
                .state
                .input_processor
                .composer_drafts()
                .map(|(node, draft)| {
                    assert_eq!(draft.text(), "draft", "the kept draft");
                    node
                })
                .next()
                .unwrap();
            let tab = harness.scene().active_tab.unwrap();
            let tile = *harness.scene().tiles.keys().next().unwrap();
            {
                let mut scene = harness.scene();
                let state = &mut harness.app.state;
                state.input_processor.recover_composer_focus(
                    &mut state.focus_manager,
                    &mut scene,
                    tab,
                    tile,
                    composer,
                );
            }
            let (text, cursor, anchor, ..) = harness
                .app
                .state
                .input_processor
                .composer_draft_snapshot()
                .unwrap();
            assert_eq!((text.as_str(), cursor, anchor), ("draft", 3, 3));
        }

        // A digit URL uses the same per-action 2s limiter, keeps the card, and
        // leaves the keyboard visit even when the second launch is refused.
        {
            let mut harness = approve_reject_notification_harness();
            {
                let mut scene = harness.scene();
                let tze_hud_scene::ZoneContent::Notification(n) = &mut scene
                    .zone_registry
                    .active_publishes
                    .get_mut("notification-area")
                    .unwrap()[0]
                    .content
                else {
                    unreachable!()
                };
                n.actions[1].callback_id = None;
                n.actions[1].url = Some("https://example.com/digit".into());
            }
            assert!(harness.tick());
            let (tx, rx) = std::sync::mpsc::channel();
            harness.app.state.notification_link_opener.worker = Some(Arc::new(move |url| {
                tx.send(url).unwrap();
                Ok(())
            }));
            request_answer(&mut harness);
            press(&mut harness, "Digit2", "2", false);
            assert_eq!(
                rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
                "https://example.com/digit"
            );
            // R3.2 S6: the digit's button shows the same feedback.
            let (target, _) =
                settle_link_to(&mut harness, tze_hud_scene::LinkFeedbackState::Opened);
            assert!(matches!(
                target,
                tze_hud_scene::LinkFeedbackTarget::CardAction { index: 1, .. }
            ));
            assert!(harness.app.state.answer_keyboard.focus.is_none());
            assert_eq!(
                harness
                    .scene()
                    .zone_registry
                    .active_for_zone("notification-area")
                    .len(),
                1
            );
            request_answer(&mut harness);
            press(&mut harness, "Digit2", "2", false);
            assert!(harness.app.state.answer_keyboard.focus.is_none());
            assert!(rx.try_recv().is_err());
            assert!(take_action_ids(&harness).is_empty());
            for worker in harness.app.state.notification_link_opener.workers.drain(..) {
                worker.join().unwrap();
            }
        }
        // Links inherit the same reverse-entry/focus and Enter/Space path.
        for (code, key) in [("Enter", "Enter"), ("Space", " ")] {
            let clock = Arc::new(tze_hud_scene::TestClock::new(1_000));
            let mut harness = approve_reject_notification_harness_on(clock.clone());
            {
                let mut scene = harness.scene();
                let tze_hud_scene::ZoneContent::Notification(n) = &mut scene
                    .zone_registry
                    .active_publishes
                    .get_mut("notification-area")
                    .unwrap()[0]
                    .content
                else {
                    unreachable!()
                };
                n.actions[3].callback_id = None;
                n.actions[3].url = Some("https://example.com/key".into());
            }
            assert!(harness.tick());
            let (tx, rx) = std::sync::mpsc::channel();
            harness.app.state.notification_link_opener.worker = Some(Arc::new(move |url| {
                tx.send(url).unwrap();
                Ok(())
            }));
            press(&mut harness, "Tab", "Tab", true);
            let focused = harness.scene().overlay.notification_focus.clone();
            assert!(focused.as_deref().unwrap().ends_with(":3"));
            press(&mut harness, code, key, false);
            assert_eq!(
                rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
                "https://example.com/key"
            );
            let attempts = harness.app.state.notification_link_opener.workers.len();
            press(&mut harness, code, key, false);
            assert_eq!(
                harness.app.state.notification_link_opener.workers.len(),
                attempts
            );
            assert!(rx.try_recv().is_err());
            assert_eq!(harness.scene().overlay.notification_focus, focused);
            assert!(take_action_ids(&harness).is_empty());
            assert!(harness.scene().overlay.notification_choices.is_empty());
            clock.advance(2000);
            harness
                .app
                .state
                .safe_mode_atomic
                .store(true, std::sync::atomic::Ordering::Release);
            press(&mut harness, code, key, false);
            assert_eq!(
                harness.app.state.notification_link_opener.workers.len(),
                attempts
            );
            assert!(rx.try_recv().is_err());
            harness
                .app
                .state
                .safe_mode_atomic
                .store(false, std::sync::atomic::Ordering::Release);
            for worker in harness.app.state.notification_link_opener.workers.drain(..) {
                worker.join().unwrap();
            }
        }
        for (code, key) in [("Enter", "Enter"), ("Space", " ")] {
            let mut harness = approve_reject_notification_harness();
            let (zone, content) = {
                let scene = harness.scene();
                (
                    scene.zone_registry.zones["notification-area"].clone(),
                    scene.zone_registry.active_for_zone("notification-area")[0]
                        .content
                        .clone(),
                )
            };
            let tab = harness.focus_composer();
            let (tile, composer) = match harness.app.state.focus_manager.current_owner(tab) {
                tze_hud_input::FocusOwner::Node { tile_id, node_id } => (*tile_id, *node_id),
                _ => unreachable!(),
            };
            let mut controls = Vec::new();
            {
                let mut scene = harness.scene();
                for (i, name) in ["minimize", "submit"].into_iter().enumerate() {
                    let id = SceneId::new();
                    scene
                        .add_node_to_tile(
                            tile,
                            Some(composer),
                            Node {
                                id,
                                layout: Default::default(),
                                children: vec![],
                                data: NodeData::HitRegion(HitRegionNode {
                                    bounds: Rect::new(10.0 + i as f32 * 80.0, 80.0, 60.0, 30.0),
                                    interaction_id: name.into(),
                                    accepts_focus: true,
                                    accepts_pointer: true,
                                    ..Default::default()
                                }),
                            },
                        )
                        .unwrap();
                    controls.push(id);
                }
                scene
                    .register_tile_scroll_config(tile, tze_hud_scene::TileScrollConfig::vertical())
                    .unwrap();
                scene.register_zone(zone);
                scene
                    .publish_to_zone(
                        "notification-area",
                        content,
                        "notif-agent",
                        None,
                        None,
                        None,
                    )
                    .unwrap();
                assert!(
                    scene.overlay.notification_focus.is_none(),
                    "publishing never steals focus"
                );
            }
            assert!(harness.tick());

            // Existing portal stop order is preserved before the card's X.
            for expected in &controls {
                press(&mut harness, "Tab", "Tab", false);
                assert!(
                    matches!(harness.app.state.focus_manager.current_owner(tab), tze_hud_input::FocusOwner::Node { node_id, .. } if node_id == expected)
                );
            }
            let saved = harness.app.state.focus_manager.current_owner(tab).clone();
            press(&mut harness, "Tab", "Tab", false);
            let dismiss = harness.scene().overlay.notification_focus.clone().unwrap();
            assert!(dismiss.contains(":dismiss:"));
            press(&mut harness, "Tab", "Tab", true);
            assert!(harness.scene().overlay.notification_focus.is_none());
            assert_eq!(
                harness.app.state.focus_manager.current_owner(tab),
                &saved,
                "reversing X returns to submit without skipping"
            );
            press(&mut harness, "Tab", "Tab", false);
            for _ in 0..4 {
                press(&mut harness, "Tab", "Tab", false);
            }
            assert!(
                harness
                    .scene()
                    .overlay
                    .notification_focus
                    .as_deref()
                    .unwrap()
                    .ends_with(":3")
            );
            press(&mut harness, "Tab", "Tab", false);
            assert!(harness.scene().overlay.notification_focus.is_none());
            assert!(
                matches!(harness.app.state.focus_manager.current_owner(tab), tze_hud_input::FocusOwner::Node { node_id, .. } if *node_id == composer)
            );
            // Reverse entry and reversing again returns directly to the composer.
            press(&mut harness, "Tab", "Tab", true);
            assert!(
                harness
                    .scene()
                    .overlay
                    .notification_focus
                    .as_deref()
                    .unwrap()
                    .ends_with(":3")
            );
            press(&mut harness, "Tab", "Tab", false);
            assert!(harness.scene().overlay.notification_focus.is_none());
            harness.enqueue(character("x", 2_000));
            harness.drain();
            assert_eq!(harness.composer_draft().as_deref(), Some("x"));
            press(&mut harness, "Tab", "Tab", true);
            // Pointer press and keyboard activation interleave on the same exact publication.
            let point = harness.notification_action_center("approve").unwrap();
            harness.app.state.cursor_x = point.0;
            harness.app.state.cursor_y = point.1;
            harness.app.enqueue_pointer_event(PointerEventKind::Down);
            // Pointer clears local focus. Re-enter reverse from the composer's boundary.
            press(&mut harness, "Tab", "Tab", true);

            let down = key_down(code, key, 3_000);
            harness.enqueue(down);
            let mut repeated = key_down(code, key, 3_100);
            if let PendingKeyboardEvent::KeyDown(raw) = &mut repeated {
                raw.repeat = true;
            }
            harness.enqueue(repeated);
            harness.enqueue(character(" ", 3_110));
            harness.enqueue(key_up(code, key, 3_200));
            harness.drain();
            harness.app.enqueue_pointer_event(PointerEventKind::Up);
            assert_eq!(take_action_ids(&harness), ["details"]);

            assert!(harness.scene().overlay.notification_focus.is_none());
            harness.enqueue(character("y", 4_000));
            harness.drain();
            assert_eq!(
                harness.composer_draft().as_deref(),
                Some("xy"),
                "editing resumes at the still-valid owner"
            );
        }

        // Keyboard focus belongs to the HUD window even when its pointer is
        // over another overlay (or no HUD window). Drive the production focus
        // event seam, including both existing nonblocking cleanup boundaries.
        for cursor_window in [None, Some(winit::window::WindowId::from(22))] {
            for busy_scene in [false, true] {
                for character_first in [false, true] {
                    let mut harness = approve_reject_notification_harness();
                    let (zone, content) = {
                        let scene = harness.scene();
                        (
                            scene.zone_registry.zones["notification-area"].clone(),
                            scene.zone_registry.active_for_zone("notification-area")[0]
                                .content
                                .clone(),
                        )
                    };
                    let tab = harness.focus_composer();
                    let owner = harness.app.state.focus_manager.current_owner(tab).clone();
                    {
                        let mut scene = harness.scene();
                        scene.register_zone(zone);
                        scene
                            .publish_to_zone(
                                "notification-area",
                                content,
                                "notif-agent",
                                None,
                                None,
                                None,
                            )
                            .unwrap();
                    }
                    assert!(harness.tick());
                    harness.enqueue(character("kept", 5_000));
                    harness.drain();

                    let mut tab_down = key_down("Tab", "Tab", 5_100);
                    if let PendingKeyboardEvent::KeyDown(raw) = &mut tab_down {
                        raw.modifiers.shift = true;
                    }
                    harness.enqueue(tab_down);
                    harness.drain();
                    assert!(harness.scene().overlay.notification_focus.is_some());
                    harness.app.state.cursor_window = cursor_window;
                    assert!(harness.app.state.press_window.is_none());
                    let shared = harness.shared_state();
                    let scene_handle = Arc::clone(&shared.blocking_lock().scene);
                    let held_shared = (!busy_scene).then(|| shared.blocking_lock());
                    let held_scene = busy_scene.then(|| scene_handle.blocking_lock());
                    harness.app.handle_window_focus(None, false);
                    assert!(harness.app.settle_scene_work().is_deferred());
                    assert!(
                        harness
                            .app
                            .state
                            .input_processor
                            .chrome_feedback_cancel_pending
                    );
                    drop(held_scene);
                    drop(held_shared);
                    if busy_scene {
                        assert!(harness.tick());
                    }
                    harness.app.handle_window_focus(None, true);
                    harness.enqueue(key_up("Tab", "Tab", 5_200));
                    harness.drain();
                    assert!(harness.app.state.notification_keydowns.is_empty());

                    assert_eq!(harness.app.state.focus_manager.current_owner(tab), &owner);
                    // A returned activation key has no stale card target. Editing
                    // resumes at the preserved owner until deliberate navigation.
                    if !character_first {
                        press(&mut harness, "Space", " ", false);
                    }
                    // Character-first return must also settle deferred chrome cleanup.
                    // In the Space-first case, named Space already edits the draft.
                    harness.enqueue(character("resumed", 5_300));
                    harness.drain();
                    assert!(take_action_ids(&harness).is_empty());
                    assert_eq!(
                        harness.composer_draft().as_deref(),
                        Some(if character_first {
                            "keptresumed"
                        } else {
                            "kept resumed"
                        })
                    );
                    assert!(harness.scene().overlay.notification_focus.is_none());
                    assert!(harness.tick());
                    assert!(
                        !harness
                            .app
                            .state
                            .input_processor
                            .chrome_feedback_cancel_pending
                    );
                    assert!(
                        harness
                            .scene()
                            .overlay
                            .chrome_states
                            .values()
                            .all(|state| !state.focused)
                    );
                    press(&mut harness, "Tab", "Tab", true);
                    press(&mut harness, "Enter", "Enter", false);
                    assert_eq!(take_action_ids(&harness), ["details"]);
                }
            }
        }

        // Hiding a focused card removes its controls immediately without
        // answering it, and traversal cannot re-enter those hidden controls.
        let mut harness = approve_reject_notification_harness();
        press(&mut harness, "Tab", "Tab", false);
        assert!(harness.scene().overlay.notification_focus.is_some());
        {
            let mut scene = harness.scene();
            scene.set_owner_hidden(tze_hud_scene::graph::SurfaceClasses {
                zones: true,
                ..Default::default()
            });
            assert!(scene.overlay.notification_focus.is_none());
            assert!(scene.overlay.zone_hit_regions.is_empty());
        }
        for reverse in [false, true] {
            press(&mut harness, "Tab", "Tab", reverse);
            press(&mut harness, "Enter", "Enter", false);
            assert!(harness.scene().overlay.notification_focus.is_none());
            assert!(take_action_ids(&harness).is_empty());
        }
        assert_eq!(
            harness
                .scene()
                .zone_registry
                .active_for_zone("notification-area")
                .len(),
            1
        );

        for (code, key) in [("Enter", "Enter"), ("Space", " ")] {
            for terminal in ["replace", "clear", "expiry"] {
                let mut harness = approve_reject_notification_harness();
                let content = harness
                    .scene()
                    .zone_registry
                    .active_for_zone("notification-area")[0]
                    .content
                    .clone();
                {
                    let mut scene = harness.scene();
                    scene.clear_zone("notification-area").unwrap();
                    scene
                        .publish_to_zone(
                            "notification-area",
                            content.clone(),
                            "notif-agent",
                            Some("slot".into()),
                            None,
                            None,
                        )
                        .unwrap();
                }
                assert!(harness.tick());
                {
                    let mut scene = harness.scene();
                    let control = scene
                        .overlay
                        .zone_hit_regions
                        .iter()
                        .find(|r| matches!(&r.kind, ZoneInteractionKind::Action { index: 0 }))
                        .unwrap()
                        .interaction_id
                        .clone();
                    scene.focus_notification(Some(control));
                }
                harness.app.state.global_tokens = tze_hud_config::themes::resolve_config_tokens(
                    &std::collections::HashMap::from([
                        ("theme".into(), "blueprint".into()),
                        ("motion.exit.ms".into(), "240".into()),
                    ]),
                );
                harness.enqueue(key_down(code, key, 2_000));
                harness.drain(); // Keyboard activation commits on Down.
                {
                    let mut scene = harness.scene();
                    if terminal == "replace" {
                        scene
                            .publish_to_zone(
                                "notification-area",
                                content,
                                "notif-agent",
                                Some("slot".into()),
                                None,
                                None,
                            )
                            .unwrap();
                    } else if terminal == "clear" {
                        scene
                            .clear_notification_key("notification-area", "notif-agent", "slot")
                            .unwrap();
                    } else {
                        scene
                            .zone_registry
                            .active_publishes
                            .get_mut("notification-area")
                            .unwrap()[0]
                            .expires_at_wall_us = Some(0);
                        scene.drain_expired_zone_publications();
                    }
                }
                assert!(harness.tick());
                let mut repeat = key_down(code, key, 2_100);
                if let PendingKeyboardEvent::KeyDown(raw) = &mut repeat {
                    raw.repeat = true;
                }
                harness.enqueue(repeat);
                harness.enqueue(key_up(code, key, 2_200));
                harness.drain();
                let accepted = harness.scene().take_pending_actions("notif-agent");
                assert_eq!(
                    accepted.len(),
                    1,
                    "{terminal} cannot turn repeat/Up into a B answer or retire accepted A"
                );
                assert_eq!(accepted[0].key.as_deref(), Some("slot"));
                assert_eq!(
                    accepted[0].kind,
                    tze_hud_scene::graph::PendingActionKind::Callback("approve".into())
                );
            }
        }
    }

    /// production proof: keyboard is a concrete pointer-free
    /// command source, not merely a library/test fixture. Drive the real pending
    /// keyboard drain and require ACTIVATE pressed feedback and matching release.
    #[test]
    fn real_keyboard_drain_applies_activate_feedback_and_matching_release() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, node_id) = install_button(&mut harness, true);

        harness.enqueue(key_down("Enter", "Enter", 1_000));
        harness.drain();

        let shared = harness.app.state.shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        assert!(
            scene
                .hit_region_states
                .get(&node_id)
                .is_some_and(|state| state.pressed),
            "ACTIVATE must set local pressed feedback"
        );
        drop(scene);
        drop(shared);

        harness.enqueue(key_up("Enter", "Enter", 2_000));
        harness.drain();

        let shared = harness.app.state.shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        assert!(
            scene
                .hit_region_states
                .get(&node_id)
                .is_some_and(|state| !state.pressed),
            "matching activation KeyUp must clear local pressed feedback"
        );
    }

    /// PageDown is focus-routed command input, not a
    /// pointer-position scroll. The focused tile must receive both local scroll
    /// feedback even when the cursor rests over another tile.
    #[test]
    fn page_down_scrolls_focused_tile_not_tile_under_pointer() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (focused_tile, pointer_tile) =
            install_conflicting_focus_and_pointer_scroll_tiles(&mut harness);

        harness.enqueue(key_down("PageDown", "PageDown", 1_000));
        harness.drain();

        let shared = harness.app.state.shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        let (_, focused_offset_y) = scene.tile_scroll_offset_local(focused_tile);
        let (_, pointer_offset_y) = scene.tile_scroll_offset_local(pointer_tile);
        assert_eq!(
            focused_offset_y,
            tze_hud_input::KEYBOARD_PAGE_SCROLL_PX,
            "PageDown must apply local-first scroll to the focused tile"
        );
        assert_eq!(
            pointer_offset_y, 0.0,
            "PageDown must not scroll the different tile under the pointer"
        );
        drop(scene);
        drop(shared);
    }

    /// PageUp shares the focus-routed ownership contract with PageDown. Seed
    /// both tiles away from origin so scrolling the wrong one is observable.
    #[test]
    fn page_up_scrolls_focused_tile_not_tile_under_pointer() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (focused_tile, pointer_tile) =
            install_conflicting_focus_and_pointer_scroll_tiles(&mut harness);
        let shared_state = Arc::clone(&harness.app.state.shared_state);
        {
            let shared = shared_state.blocking_lock();
            let mut scene = shared.scene.blocking_lock();
            harness.app.state.input_processor.process_keyboard_scroll(
                focused_tile,
                tze_hud_input::KEYBOARD_PAGE_SCROLL_PX,
                &mut scene,
            );
            harness.app.state.input_processor.process_keyboard_scroll(
                pointer_tile,
                tze_hud_input::KEYBOARD_PAGE_SCROLL_PX,
                &mut scene,
            );
        }

        harness.enqueue(key_down("PageUp", "PageUp", 1_000));
        harness.drain();

        let shared = shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        let (_, focused_offset_y) = scene.tile_scroll_offset_local(focused_tile);
        let (_, pointer_offset_y) = scene.tile_scroll_offset_local(pointer_tile);
        assert_eq!(
            focused_offset_y, 0.0,
            "PageUp must apply local-first scroll to the focused tile"
        );
        assert_eq!(
            pointer_offset_y,
            tze_hud_input::KEYBOARD_PAGE_SCROLL_PX,
            "PageUp must leave the different tile under the pointer unchanged"
        );
        drop(scene);
        drop(shared);
    }

    /// The keyboard correction must not alter wheel ownership: wheel input is
    /// still pointer-routed even when keyboard focus belongs to another tile.
    #[test]
    fn wheel_scroll_still_targets_tile_under_pointer_not_focused_tile() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (focused_tile, pointer_tile) =
            install_conflicting_focus_and_pointer_scroll_tiles(&mut harness);

        harness
            .app
            .enqueue_scroll_event(0.0, 40.0, tze_hud_input::WheelStep::Pixels);

        let shared = harness.app.state.shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        let (_, focused_offset_y) = scene.tile_scroll_offset_local(focused_tile);
        let (_, pointer_offset_y) = scene.tile_scroll_offset_local(pointer_tile);
        assert_eq!(
            focused_offset_y, 0.0,
            "wheel scroll must not be redirected from the pointer tile to focus"
        );
        assert_eq!(
            pointer_offset_y, 40.0,
            "wheel scroll must retain its pointer-hit-test ownership"
        );
    }

    /// A busy scene lock never loses pointer input: the Move and
    /// the wheel delta that arrive while an agent holds the scene both take
    /// effect once it frees, through the one coalesced availability wake and
    /// no periodic retry while held.
    #[test]
    fn held_scene_lock_retains_pointer_move_and_wheel_until_release() {
        use std::time::{Duration, Instant};

        let mut harness = HeadlessEventLoopHarness::new();
        let (tile_id, node_id) = install_button(&mut harness, false);
        {
            let shared = harness.app.state.shared_state.blocking_lock();
            shared
                .scene
                .blocking_lock()
                .register_tile_scroll_config(tile_id, tze_hud_scene::TileScrollConfig::vertical())
                .unwrap();
        }
        harness.app.state.cursor_x = 900.0;
        harness.app.state.cursor_y = 900.0;
        harness.app.enqueue_pointer_event(PointerEventKind::Move);

        let scene_handle = Arc::clone(&harness.app.state.shared_state.blocking_lock().scene);
        let held = scene_handle.blocking_lock();
        let main_work_before = harness.app.state.wake.main_work_generation();

        // y = 12 hits the button (local 10..58) both before and after the
        // 40 px scroll, whichever order the retained input applies in.
        harness.app.state.cursor_x = 50.0;
        harness.app.state.cursor_y = 12.0;
        harness.app.enqueue_pointer_event(PointerEventKind::Move);
        harness
            .app
            .enqueue_scroll_event(0.0, 20.0, tze_hud_input::WheelStep::Pixels);
        harness
            .app
            .enqueue_scroll_event(0.0, 20.0, tze_hud_input::WheelStep::Pixels);
        harness.app.settle_scene_work();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            harness.app.state.wake.main_work_generation(),
            main_work_before,
            "a held scene lock must cause no periodic retry wakes"
        );
        let compositor_before = harness.app.state.wake.compositor().checkpoint();
        for _ in 0..2 {
            assert!(!harness.tick());
            assert_eq!(harness.offscreen_work_counts(), (0, 0));
            assert_eq!(harness.frame_turn.last_rendered(), (u64::MAX, u64::MAX));
            assert_eq!(
                harness.app.state.wake.compositor().checkpoint(),
                compositor_before
            );
        }
        drop(held);

        let deadline = Instant::now() + Duration::from_secs(1);
        while harness.app.state.wake.main_work_generation() == main_work_before {
            assert!(
                Instant::now() < deadline,
                "releasing the scene lock must wake the main loop to retry"
            );
            std::thread::yield_now();
        }
        while harness.app.state.wake.compositor().checkpoint() == compositor_before {
            assert!(
                Instant::now() < deadline,
                "frame turn resumes on availability, never a polling cadence"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            harness.app.state.wake.compositor().checkpoint(),
            compositor_before + 1,
            "two busy turns share one compositor availability wake"
        );
        harness.app.settle_scene_work();

        let shared = harness.app.state.shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        assert!(
            scene.hit_region_states[&node_id].hovered,
            "the Move made while the scene was busy must still update hover"
        );
        assert_eq!(
            scene.tile_scroll_offset_local(tile_id).1,
            40.0,
            "both wheel deltas made while the scene was busy must apply"
        );
    }

    /// A wheel burst behind a retained click coalesces into one entry, so it
    /// can neither evict the click nor overtake it.
    #[test]
    fn held_scene_lock_wheel_burst_waits_behind_click_without_evicting_it() {
        use std::time::{Duration, Instant};

        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, _node_id) = install_button(&mut harness, false);
        let scene_handle = Arc::clone(&harness.app.state.shared_state.blocking_lock().scene);
        let held = scene_handle.blocking_lock();
        let main_work_before = harness.app.state.wake.main_work_generation();
        harness.app.state.cursor_x = 50.0;
        harness.app.state.cursor_y = 30.0;
        harness.app.enqueue_pointer_event(PointerEventKind::Down);
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        for _ in 0..40 {
            harness
                .app
                .enqueue_scroll_event(0.0, -10.0, tze_hud_input::WheelStep::Pixels);
        }
        let retained = &harness.app.state.pending_pointer_buttons;
        assert_eq!(retained.len(), 3, "the burst shares one wheel entry");
        assert!(matches!(
            &retained[0].event,
            crate::windowed::lifecycle::RetainedPointerEvent::Button {
                kind: PointerEventKind::Down,
                ..
            }
        ));
        assert!(matches!(
            &retained[2].event,
            crate::windowed::lifecycle::RetainedPointerEvent::Wheel(deltas) if deltas.len() == 1
        ));
        assert!(
            harness.app.state.pending_scroll.is_empty(),
            "wheel behind the click does not join the scroll ahead of it"
        );
        drop(held);

        let deadline = Instant::now() + Duration::from_secs(1);
        while harness.app.state.wake.main_work_generation() == main_work_before {
            assert!(Instant::now() < deadline, "release must wake the replay");
            std::thread::yield_now();
        }
        harness.app.settle_scene_work();
        assert!(harness.app.state.pending_pointer_buttons.is_empty());
        assert!(harness.app.state.pending_scroll.is_empty());
    }

    /// A click made while the scene lock is busy is not lost:
    /// the press and release retain their own positions and replay in order
    /// once the lock frees: the press focuses the button it was made on even
    /// though the pointer has since left, and the release ends the press.
    #[test]
    fn held_scene_lock_retains_click_and_replays_it_in_order() {
        use std::time::{Duration, Instant};

        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, node_id) = install_button(&mut harness, false);

        let scene_handle = Arc::clone(&harness.app.state.shared_state.blocking_lock().scene);
        let held = scene_handle.blocking_lock();
        let main_work_before = harness.app.state.wake.main_work_generation();

        // Press and release on the button (local 10..170 x 10..58)...
        harness.app.state.cursor_x = 50.0;
        harness.app.state.cursor_y = 30.0;
        harness.app.enqueue_pointer_event(PointerEventKind::Down);
        harness.app.enqueue_pointer_event(PointerEventKind::Up);
        // Typing that follows the click waits behind it, even though the
        // keyboard path itself is free.
        let PendingKeyboardEvent::Character(typed) = character("x", 2_000) else {
            unreachable!()
        };
        harness.app.dispatch_character_event(&typed);
        assert_eq!(harness.app.state.pending_keyboard_events.len(), 1);
        // ...then the pointer leaves before the lock frees.
        harness.app.state.cursor_x = 900.0;
        harness.app.state.cursor_y = 900.0;
        harness.app.enqueue_pointer_event(PointerEventKind::Move);
        assert_eq!(
            harness
                .app
                .state
                .pending_pointer_buttons
                .iter()
                .map(|b| match &b.event {
                    crate::windowed::lifecycle::RetainedPointerEvent::Button {
                        kind, x, y, ..
                    } => {
                        (kind.clone(), *x, *y)
                    }
                    other => panic!("only buttons were retained: {other:?}"),
                })
                .collect::<Vec<_>>(),
            vec![
                (PointerEventKind::Down, 50.0, 30.0),
                (PointerEventKind::Up, 50.0, 30.0)
            ],
            "a busy lock retains each press and release at its own position"
        );
        drop(held);

        let deadline = Instant::now() + Duration::from_secs(1);
        while harness.app.state.wake.main_work_generation() == main_work_before {
            assert!(
                Instant::now() < deadline,
                "releasing the scene lock must wake the main loop to replay"
            );
            std::thread::yield_now();
        }
        harness.app.settle_scene_work();

        assert!(harness.app.state.pending_pointer_buttons.is_empty());
        assert!(harness.app.state.pending_keyboard_events.is_empty());
        assert_eq!(
            (harness.app.state.cursor_x, harness.app.state.cursor_y),
            (900.0, 900.0),
            "replay restores the live cursor"
        );
        let shared = harness.app.state.shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        assert_eq!(
            harness
                .app
                .state
                .focus_manager
                .current_owner(scene.active_tab.unwrap())
                .node_id(),
            Some(node_id),
            "the retained click focuses its original button after the cursor moves away"
        );
        assert!(
            !scene.hit_region_states[&node_id].pressed,
            "the replayed release ends the press"
        );
    }

    /// A Move retained across a busy lock is dropped, not kept as deferred
    /// work, once a context-menu press owns the pointer; otherwise the free
    /// lock's availability wake would fire every turn while the button is
    /// held.
    #[test]
    fn held_scene_lock_retained_move_yields_to_context_menu_press() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, _node_id) = install_button(&mut harness, false);
        let scene_handle = Arc::clone(&harness.app.state.shared_state.blocking_lock().scene);
        let held = scene_handle.blocking_lock();
        harness.app.state.cursor_x = 50.0;
        harness.app.state.cursor_y = 12.0;
        harness.app.enqueue_pointer_event(PointerEventKind::Move);
        assert!(harness.app.state.pending_pointer_move);
        harness.app.state.context_menu_press = Some(
            super::super::widgets::ContextMenuPress::unarmed(winit::window::WindowId::dummy()),
        );
        drop(held);

        harness.app.settle_scene_work();
        assert!(
            !harness.app.state.pending_pointer_move,
            "a moot retained Move must not stay deferred scene work"
        );
    }

    /// Held wheel input keeps its direction changes: 60 px past the tail then
    /// 3 px back must still scroll back, not net out to "stay at tail".
    #[test]
    fn held_scene_lock_retained_wheel_keeps_a_reversal_after_the_tail() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (tile_id, _node_id) = install_button(&mut harness, false);
        {
            let shared = harness.app.state.shared_state.blocking_lock();
            shared
                .scene
                .blocking_lock()
                .register_tile_scroll_config(
                    tile_id,
                    tze_hud_scene::TileScrollConfig {
                        content_height: Some(500.0),
                        ..tze_hud_scene::TileScrollConfig::vertical()
                    },
                )
                .unwrap();
        }
        harness.app.state.cursor_x = 50.0;
        harness.app.state.cursor_y = 12.0;
        harness
            .app
            .enqueue_scroll_event(0.0, 1.0e6, tze_hud_input::WheelStep::Pixels);
        let tail = 500.0;

        let scene_handle = Arc::clone(&harness.app.state.shared_state.blocking_lock().scene);
        let held = scene_handle.blocking_lock();
        let main_work_before = harness.app.state.wake.main_work_generation();
        harness
            .app
            .enqueue_scroll_event(0.0, 60.0, tze_hud_input::WheelStep::Pixels);
        harness
            .app
            .enqueue_scroll_event(0.0, -3.0, tze_hud_input::WheelStep::Pixels);
        drop(held);
        // Retry on the release-driven wake, as the event loop does: until it
        // fires, the availability waiter may itself hold the scene briefly.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while harness.app.state.wake.main_work_generation() == main_work_before {
            assert!(
                std::time::Instant::now() < deadline,
                "releasing the scene lock must wake the main loop to retry"
            );
            std::thread::yield_now();
        }
        harness.app.settle_scene_work();
        assert!(harness.app.state.pending_scroll.is_empty());

        let shared = harness.app.state.shared_state.blocking_lock();
        let offset = shared
            .scene
            .blocking_lock()
            .tile_scroll_offset_local(tile_id)
            .1;
        assert_eq!(
            offset,
            tail - 3.0,
            "the clamp applies to the 60 px before the 3 px reversal"
        );
    }

    /// ArrowDown remains an abstract command binding at tile focus, but this
    /// PageUp/PageDown correction must not invent a page-sized local side
    /// effect for that distinct key.
    #[test]
    fn arrow_down_command_does_not_apply_page_scroll_side_effect() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (focused_tile, pointer_tile) =
            install_conflicting_focus_and_pointer_scroll_tiles(&mut harness);

        harness.enqueue(key_down("ArrowDown", "ArrowDown", 1_000));
        harness.drain();

        let shared = harness.app.state.shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        assert_eq!(
            scene.tile_scroll_offset_local(focused_tile).1,
            0.0,
            "ArrowDown must not reuse the page-scroll side effect"
        );
        assert_eq!(
            scene.tile_scroll_offset_local(pointer_tile).1,
            0.0,
            "ArrowDown must not scroll the pointer tile either"
        );
        drop(scene);
        drop(shared);
    }

    /// The real keyboard drain must apply Tab navigation to the focused owner.
    #[test]
    fn real_keyboard_drain_moves_focus_to_next_owner() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, node_id) = install_button(&mut harness, false);

        harness.enqueue(key_down("Tab", "Tab", 1_000));
        harness.drain();

        assert_eq!(
            harness
                .app
                .state
                .focus_manager
                .current_owner(
                    *harness
                        .app
                        .state
                        .active_tab_mirror
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                )
                .node_id(),
            Some(node_id),
            "Tab must move focus locally"
        );
    }

    /// Composer-less Escape must clear the focused control through the real drain.
    #[test]
    fn real_keyboard_drain_clears_composerless_focus_on_cancel() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, _node_id) = install_button(&mut harness, true);

        harness.enqueue(key_down("Escape", "Escape", 1_000));
        harness.drain();

        let tab_id = harness
            .app
            .state
            .active_tab_mirror
            .lock()
            .unwrap()
            .expect("test scene has an active tab");
        assert_eq!(
            *harness.app.state.focus_manager.current_owner(tab_id),
            tze_hud_input::FocusOwner::None,
            "Escape recovery must still clear the composer-less focus stop"
        );
    }

    /// headline: the keyboard-drain full path runs end-to-end through
    /// production dispatch — NOT a reconstructed closure.
    ///
    /// A focused composer plus a pile of queued character events is drained by
    /// the real [`WinitApp::drain_pending_keyboard_events`]. That method resolves
    /// the active tab from the lock-free mirror, pops each event, and routes it
    /// through `dispatch_character_event_inner` (the composer intercept). We
    /// assert the queue fully drains AND every keystroke landed in the real
    /// `InputProcessor` composer draft — the exact end-to-end behavior the old
    /// hand-reconstructed drain loop only *modeled*.
    #[test]
    fn drain_routes_characters_through_real_dispatch_into_composer_draft() {
        let mut harness = HeadlessEventLoopHarness::new();
        harness.focus_composer();

        for (i, ch) in ["h", "e", "l", "l", "o"].into_iter().enumerate() {
            harness.enqueue(character(ch, (i as u64 + 1) * 1_000));
        }
        assert_eq!(harness.pending_len(), 5, "precondition: 5 events queued");

        harness.drain();

        assert_eq!(
            harness.pending_len(),
            0,
            "the real drain must fully empty the queue (no front→back rotation)"
        );
        assert_eq!(
            harness.composer_draft().as_deref(),
            Some("hello"),
            "every drained keystroke must be applied to the composer draft via \
             the real dispatch path"
        );
    }

    /// The real drain must stop immediately — popping nothing — when the
    /// lock-free `active_tab_mirror` is contended, preserving strict FIFO order
    /// across the `about_to_wait` boundary.
    ///
    /// This exercises the `active_tab_for_keyboard_dispatch().is_none()` break at
    /// the top of the genuine drain loop. `std::sync::Mutex::try_lock` returns
    /// `WouldBlock` while a guard is held (even on the same thread), so holding
    /// the mirror guard here forces that busy branch without a second thread.
    #[test]
    fn drain_breaks_without_popping_when_active_tab_mirror_is_busy() {
        let mut harness = HeadlessEventLoopHarness::new();
        harness.focus_composer();
        harness.enqueue(character("a", 1_000));
        harness.enqueue(character("b", 2_000));

        let mirror = harness.active_tab_mirror();
        let guard = mirror.try_lock().expect("mirror must be free before drain");

        harness.drain();

        drop(guard);
        assert_eq!(
            harness.pending_len(),
            2,
            "no event may be popped while the active_tab mirror is busy"
        );
        // Draft untouched — nothing was dispatched.
        assert_eq!(
            harness.composer_draft().as_deref(),
            Some(""),
            "no keystroke may reach the composer draft while the mirror is busy"
        );
    }

    /// When inner dispatch defers the popped event (a required shared-state/scene
    /// lock was busy), the real drain must restore that event to the FRONT of the
    /// queue and break — never let a later event overtake it.
    ///
    /// The composer intercept in `dispatch_character_event_inner` resolves its
    /// delivery context via `namespace_for_keyboard_tile`, which `try_lock`s
    /// `shared_state`. Holding that guard forces `ComposerTargetLookup::
    /// Busy`, so the inner fn pushes the popped event to the tail;
    /// `restore_front_requeued_event` inside the genuine drain then detects the
    /// growth, moves it back to the front, and stops. This is the real-path
    /// analogue of the previously reconstructed `restore_front_requeued_event`
    /// closure test.
    #[test]
    fn drain_restores_requeued_event_to_front_when_delivery_context_busy() {
        let mut harness = HeadlessEventLoopHarness::new();
        harness.focus_composer();
        harness.enqueue(character("x", 1_000));
        harness.enqueue(character("y", 2_000));

        // Hold shared_state so namespace_for_keyboard_tile → Busy inside the
        // inner composer dispatch. try_lock is non-reentrant, so holding the
        // guard on this thread is sufficient.
        let shared = harness.shared_state();
        let guard = shared
            .try_lock()
            .expect("shared_state must be free before drain");

        harness.drain();

        drop(guard);

        // The popped-then-deferred "x" must be back at the front, ahead of "y",
        // and nothing may have been consumed into the draft.
        assert_eq!(
            harness.pending_len(),
            2,
            "the deferred event must be restored, not dropped"
        );
        match harness.front_pending() {
            Some(PendingKeyboardEvent::Character(raw)) => assert_eq!(
                raw.character, "x",
                "the deferred event must be restored to the FRONT (FIFO preserved)"
            ),
            other => panic!("expected Character(\"x\") at front, got {other:?}"),
        }
        assert_eq!(
            harness.composer_draft().as_deref(),
            Some(""),
            "a busy-deferred keystroke must not mutate the draft"
        );
    }

    /// A held shared-scene lock must not make the real keyboard drain retry on
    /// a fixed timer.  The deferred event stays parked until the one
    /// availability waiter completes; then the same production drain makes
    /// forward progress and owns exactly one compositor notification.
    #[test]
    fn held_scene_lock_retries_real_keyboard_drain_only_after_availability() {
        use std::time::{Duration, Instant};

        let mut harness = HeadlessEventLoopHarness::new();
        harness.focus_composer();
        harness.enqueue(character("x", 1_000));

        let shared = harness.shared_state();
        let guard = shared
            .try_lock()
            .expect("shared_state must be free before inducing contention");
        let compositor_before = harness.app.state.wake.compositor().checkpoint();
        let main_work_before = harness.app.state.wake.main_work_generation();

        // This is the genuine dispatch path: it preserves the event at the
        // front because the delivery context cannot acquire shared_state.
        harness.drain();
        assert_eq!(
            harness.pending_len(),
            1,
            "busy drain must preserve the event"
        );

        // This calls the same helper used by about_to_wait.  While the lock is
        // held the waiter cannot complete, and no frame-cadence retry may be
        // emitted in its place.
        harness.schedule_shared_scene_availability_wake();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            harness.app.state.wake.compositor().checkpoint(),
            compositor_before,
            "held scene lock must cause zero periodic compositor retry wakes"
        );
        assert_eq!(
            harness.app.state.wake.main_work_generation(),
            main_work_before,
            "held scene lock must cause zero periodic main-thread retry wakes"
        );

        drop(guard);

        let deadline = Instant::now() + Duration::from_secs(1);
        while harness.app.state.wake.main_work_generation() == main_work_before {
            assert!(
                Instant::now() < deadline,
                "availability release must schedule forward progress"
            );
            std::thread::yield_now();
        }

        // In a real winit turn the queued wake enters about_to_wait, retries
        // the drain, and only then acknowledges the producer generation.
        harness.drain();
        let checkpoint = harness.app.state.wake.main_work_checkpoint();
        assert!(harness.app.state.wake.finish_main_work(checkpoint));
        assert_eq!(
            harness.app.state.wake.compositor().checkpoint(),
            compositor_before + 1,
            "availability completion owns one post-drain compositor generation"
        );

        assert_eq!(
            harness.pending_len(),
            0,
            "released scene lock must drain FIFO work"
        );
        assert_eq!(harness.composer_draft().as_deref(), Some("x"));
    }

    #[test]
    fn armed_cursor_entry_never_wakes_compositor_until_hover_changes() {
        use super::super::wake::CursorEntryTurn;
        use crate::window::{HitRegion, WindowMode};

        let mut harness = HeadlessEventLoopHarness::new();
        let state = &mut harness.app.state;
        state.effective_mode = WindowMode::Overlay;
        state.hit_regions = vec![HitRegion::new(100.0, 100.0, 50.0, 50.0)];
        state.cursor_x = 10.0;
        state.cursor_y = 10.0;
        let (tile_a, tile_b) = {
            let shared = harness.app.state.shared_state.blocking_lock();
            let mut scene = shared.scene.blocking_lock();
            *scene = SceneGraph::new(1920.0, 1080.0);
            let tab = scene.create_tab("Main", 0).unwrap();
            let lease = scene.grant_lease("agent", 60_000);
            let mut tile = |x: f32| {
                scene
                    .create_tile(tab, "agent", lease, Rect::new(x, 200.0, 100.0, 100.0), 5)
                    .unwrap()
            };
            let ids = (tile(200.0), tile(400.0));
            harness.app.state.pipeline.hit_test_snapshot.store(Arc::new(
                crate::pipeline::HitTestSnapshot::from_scene(&scene),
            ));
            ids
        };
        let seeded = harness.app.cursor_entry_signature();
        harness.app.state.cursor_entry.signature = Some(seeded);

        let wake = harness.app.state.wake.clone();
        let compositor_before = wake.compositor().checkpoint();
        let main_work_before = wake.main_work_generation();
        let counters_before = wake.counters().snapshot();

        // Hold the scene so a poll that touched it could not go unnoticed
        // (it would defer, and the quiet result below would not hold).
        let shared = harness.shared_state();
        let guard = shared.try_lock().expect(BUSY);
        let _scene = guard.scene.try_lock().expect(BUSY);
        for _ in 0..100 {
            let CursorEntryTurn::Quiet { next } = harness.app.cursor_entry_turn() else {
                panic!("an unchanged cursor must stay on the quiet path");
            };
            assert!(next.is_none(), "no periodic deadline is ever armed");
        }
        assert_eq!(wake.compositor().checkpoint(), compositor_before);
        assert_eq!(wake.main_work_generation(), main_work_before);
        assert_eq!(wake.counters().snapshot(), counters_before);

        // A FrameReady user event can be followed by raw mouse motion before
        // about_to_wait. An unchanged cursor must not suppress present-only work.
        let sender = harness.app.state.frame_ready_tx.as_ref().unwrap().clone();
        sender.send(true).unwrap();
        harness.app.state.wake.take_main_source();
        harness.app.sync_cursor_entry_listening();
        harness
            .app
            .on_device_event(&winit::event::DeviceEvent::MouseMotion { delta: (1.0, 0.0) });
        assert!(harness.app.state.cursor_entry.tick);
        assert_eq!(
            harness.app.cursor_entry_turn(),
            CursorEntryTurn::FullTurn,
            "ready image bypasses cursor-only early return"
        );
        assert_eq!(wake.main_work_generation(), main_work_before);
        harness.app.maybe_present_frame_with(|| Some(false));
        assert!(!harness.app.state.frame_ready_rx.has_changed().unwrap());
        harness
            .app
            .on_device_event(&winit::event::DeviceEvent::MouseMotion { delta: (1.0, 0.0) });
        assert_eq!(
            harness.app.cursor_entry_turn(),
            CursorEntryTurn::FullTurn,
            "deferred image also bypasses cursor-only early return"
        );
        assert_eq!(wake.main_work_generation(), main_work_before);
        harness.app.maybe_present_frame_with(|| None);
        assert!(matches!(
            harness.app.cursor_entry_turn(),
            CursorEntryTurn::Quiet { next: None }
        ));
        assert_eq!(wake.compositor().checkpoint(), compositor_before);

        // Entering a region is a hover change: one full turn, one wake.
        harness.app.state.cursor_x = 120.0;
        harness.app.state.cursor_y = 120.0;
        assert_eq!(harness.app.cursor_entry_turn(), CursorEntryTurn::FullTurn);
        assert_eq!(wake.main_work_generation(), main_work_before + 1);
        assert!(wake.finish_main_work(wake.main_work_checkpoint()));
        assert_eq!(wake.compositor().checkpoint(), compositor_before + 1);

        // Capture flip (passthrough -> capturing) disarms raw input.
        harness.app.state.overlay_capturing = true;
        assert!(!harness.app.cursor_entry_armed());
        assert!(matches!(
            harness.app.cursor_entry_turn(),
            CursorEntryTurn::Quiet { next: None }
        ));
        assert_eq!(wake.compositor().checkpoint(), compositor_before + 1);

        // Leaving the region is the next change: exactly one more wake.
        harness.app.state.cursor_x = 10.0;
        harness.app.state.overlay_capturing = false;
        assert_eq!(harness.app.cursor_entry_turn(), CursorEntryTurn::FullTurn);
        assert!(wake.finish_main_work(wake.main_work_checkpoint()));
        assert_eq!(wake.compositor().checkpoint(), compositor_before + 2);

        // The quiet path skips `push_tile_close_hover`, so the close target is
        // part of the signature: entering, switching, and leaving a dismissible
        // tile (or the window) is one full turn and one wake each; moving
        // within the same target costs none.
        let mut expected = compositor_before + 2;
        let mut step = |app: &mut WinitApp, x: f32, left: bool, target: Option<SceneId>, wakes| {
            app.state.cursor_x = x;
            app.state.cursor_y = 250.0;
            app.state.cursor_left_window = left;
            assert_eq!(app.close_hover_target(), target);
            let turn = app.cursor_entry_turn();
            if wakes {
                assert_eq!(turn, CursorEntryTurn::FullTurn);
                assert!(
                    app.state
                        .wake
                        .finish_main_work(app.state.wake.main_work_checkpoint())
                );
                expected += 1;
            } else {
                assert!(matches!(turn, CursorEntryTurn::Quiet { .. }));
            }
            assert_eq!(app.state.wake.compositor().checkpoint(), expected);
        };
        step(&mut harness.app, 250.0, false, Some(tile_a), true);
        step(&mut harness.app, 260.0, false, Some(tile_a), false);
        step(&mut harness.app, 450.0, false, Some(tile_b), true);
        step(&mut harness.app, 450.0, true, None, true);
        step(&mut harness.app, 10.0, true, None, false);

        // A passthrough body exposes the move pill without capturing the body
        // or emitting input. Only entering/leaving changes the wake signature.
        let (mut scene, tile, _, _) =
            super::super::test_support::scene_with_drag_handle_tile(600.0, 200.0, 300.0, 200.0);
        scene.tiles.get_mut(&tile).unwrap().input_mode = tze_hud_scene::InputMode::Passthrough;
        scene.overlay.drag_handle_hit_regions[0].element_bounds = Some(scene.tiles[&tile].bounds);
        harness.app.state.pipeline.hit_test_snapshot.store(Arc::new(
            crate::pipeline::HitTestSnapshot::from_scene(&scene),
        ));
        harness.app.state.cursor_x = 700.0;
        harness.app.state.cursor_y = 300.0;
        harness.app.state.cursor_left_window = false;
        let signature = harness.app.cursor_entry_signature();
        assert_eq!(signature.drag_handle, Some(tile));
        assert!(!signature.capture);
        assert_eq!(harness.app.cursor_entry_turn(), CursorEntryTurn::FullTurn);
        harness.app.push_drag_handle_hover();
        assert_eq!(
            *harness.app.state.drag_handle_hover_state.lock().unwrap(),
            Some(tile)
        );
        assert!(wake.finish_main_work(wake.main_work_checkpoint()));
        let before = wake.compositor().checkpoint();
        harness.app.state.cursor_x += 10.0;
        assert!(matches!(
            harness.app.cursor_entry_turn(),
            CursorEntryTurn::Quiet { next: None }
        ));
        assert_eq!(wake.compositor().checkpoint(), before);
        harness.app.state.cursor_left_window = true;
        assert_eq!(harness.app.cursor_entry_turn(), CursorEntryTurn::FullTurn);
        harness.app.push_drag_handle_hover();
        assert_eq!(
            *harness.app.state.drag_handle_hover_state.lock().unwrap(),
            None
        );
        assert!(wake.finish_main_work(wake.main_work_checkpoint()));
    }

    /// The device-event wiring: only mouse motion ticks, and arming/disarming
    /// yields the listen mode exactly once per edge.
    #[test]
    fn device_event_wiring_routes_motion_and_syncs_listening() {
        use crate::window::{HitRegion, WindowMode};
        use winit::event::{DeviceEvent, ElementState};
        use winit::event_loop::DeviceEvents;

        let mut harness = HeadlessEventLoopHarness::new();
        let app = &mut harness.app;
        app.state.effective_mode = WindowMode::Overlay;
        app.state.hit_regions = vec![HitRegion::new(100.0, 100.0, 50.0, 50.0)];
        app.state.overlay_capturing = false;

        assert_eq!(
            app.sync_cursor_entry_listening(),
            Some(DeviceEvents::Always)
        );
        assert_eq!(app.sync_cursor_entry_listening(), None);

        app.on_device_event(&DeviceEvent::Button {
            button: 0,
            state: ElementState::Pressed,
        });
        assert!(!app.state.cursor_entry.tick, "non-motion is ignored");
        app.on_device_event(&DeviceEvent::MouseMotion { delta: (1.0, 0.0) });
        assert!(app.state.cursor_entry.tick);

        app.state.overlay_capturing = true;
        assert_eq!(app.sync_cursor_entry_listening(), Some(DeviceEvents::Never));
    }

    /// Raw mouse motion queues a cursor check only while armed; a due timer
    /// deadline always owes main work and never a cursor-only check.
    #[test]
    fn raw_mouse_motion_ticks_only_while_armed() {
        use super::super::wake::Deadline;
        use crate::idle_efficiency::RuntimeWakeupSource::AnimationDeadline;

        let mut harness = HeadlessEventLoopHarness::new();
        harness.app.on_raw_mouse_motion();
        assert!(!harness.app.state.cursor_entry.tick, "unarmed: ignored");

        harness.app.state.cursor_entry.listening = true;
        harness.app.on_raw_mouse_motion();
        assert!(harness.app.state.cursor_entry.tick);

        harness.app.state.cursor_entry.tick = false;
        let now = std::time::Instant::now();
        let before = harness.app.state.wake.main_work_generation();
        harness.app.state.scheduled_main_deadline = Some(Deadline::new(now, AnimationDeadline));
        harness.app.on_resume_time_reached(now);
        assert!(!harness.app.state.cursor_entry.tick);
        assert_eq!(harness.app.state.wake.main_work_generation(), before + 1);
    }

    /// A quit must leave winit's loop without another OS message: winit on
    /// Windows checks `exit()` only after the wait that follows
    /// `about_to_wait`, so the exit must also stop that wait.
    #[tokio::test]
    async fn overlay_close_is_ignored_but_tray_quit_and_dev_close_exit() {
        use std::cell::RefCell;
        use winit::event_loop::ControlFlow;

        #[derive(Default)]
        struct Recorder(RefCell<Vec<Option<ControlFlow>>>);
        impl crate::windowed::LoopExit for Recorder {
            fn set_control_flow(&self, control_flow: ControlFlow) {
                self.0.borrow_mut().push(Some(control_flow));
            }
            fn exit(&self) {
                self.0.borrow_mut().push(None);
            }
        }

        let mut harness = HeadlessEventLoopHarness::with_network(
            super::super::WindowedConfig::default(),
            Arc::new(tze_hud_scene::TestClock::new(1000)),
        )
        .await
        .expect("start the headless panel and network");
        harness.app.state.effective_mode = crate::window::WindowMode::Overlay;
        let idle = Recorder::default();
        harness.app.handle_close_request(&idle, true);
        assert!(!harness.app.state.shutdown.is_triggered());
        assert!(!harness.app.leave_if_shutdown(&idle));
        assert!(
            idle.0.borrow().is_empty(),
            "no shutdown: the loop is untouched"
        );

        let shutdown = harness.app.state.shutdown.clone();
        let mut reasons = shutdown.subscribe();
        harness.control_panel().set_quit(Arc::new(move || {
            shutdown.trigger(crate::threads::ShutdownReason::Clean);
        }));
        harness.tray_command("quit");
        let reason = tokio::time::timeout(std::time::Duration::from_secs(2), reasons.recv())
            .await
            .expect("tray Quit requests shutdown")
            .expect("shutdown reason");
        assert!(matches!(reason, crate::threads::ShutdownReason::Clean));
        let quitting = Recorder::default();
        assert!(harness.app.leave_if_shutdown(&quitting));
        assert_eq!(*quitting.0.borrow(), [Some(ControlFlow::Poll), None]);

        for (mode, windows) in [
            (crate::window::WindowMode::Fullscreen, true),
            (crate::window::WindowMode::Overlay, false),
        ] {
            let mut dev = HeadlessEventLoopHarness::new();
            dev.app.state.effective_mode = mode;
            let closing = Recorder::default();
            dev.app.handle_close_request(&closing, windows);
            assert!(dev.app.state.shutdown.is_triggered());
            assert_eq!(*closing.0.borrow(), [Some(ControlFlow::Poll), None]);
        }
    }
}
