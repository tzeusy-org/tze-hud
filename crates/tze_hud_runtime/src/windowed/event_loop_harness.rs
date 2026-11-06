//! # event_loop_harness
//!
//! Headless test harness that drives the runtime's real event/window
//! state-machine — the code production runs inside winit's
//! [`ApplicationHandler`] — with synthetic events, WITHOUT constructing a real
//! `winit` window, a `wgpu` surface, or running the OS event loop (hud-nu0ea).
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
//! This is deliberately NOT an attempt to run a real headless `WinitApp` with a
//! GPU — that is the `cargo test -p tze_hud_compositor` llvmpipe pixel-readback
//! deadlock this harness is meant to avoid. It drives only the parts of the
//! state machine that are GPU-independent (input/keyboard dispatch, portal/
//! composer routing over the shared scene). The window, surface, and compositor
//! fields are left `None`. The capture-accounting gate additionally uses a
//! headless compositor for real frame builds, substituting only GPU readback
//! and swapchain operations through production-used frame/present seams.
//!
//! ## Networked mode (POC acceptance)
//!
//! [`HeadlessEventLoopHarness::with_network`] boots the same state from a real
//! config with the production MCP HTTP server on a loopback ephemeral port and
//! the MCP → event-loop portal-op channel, plus the gRPC session server on
//! another (all over one `SharedState`), all reading one injected clock. The
//! control panel task runs too ([`HeadlessEventLoopHarness::press_chord`]).
//! [`HeadlessEventLoopHarness::tick`] runs one event-loop turn: the main-thread
//! settle sequence `about_to_wait` runs, then the compositor's Stage 4
//! timed-content sweep and notification hit-region refresh, minus the GPU.
//! `tests/integration/poc_acceptance.rs` drives it (feature `test-harness`).

use std::net::SocketAddr;
use std::sync::Arc;

#[cfg(test)]
use tze_hud_input::PointerEvent;
use tze_hud_input::{
    FocusManager, InputProcessor, KeyboardModifiers, KeyboardProcessor, PointerEventKind,
    RawCharacterEvent, RawKeyDownEvent, RawKeyUpEvent,
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
    /// - the broadcast channels (`element_repositioned_tx`, `input_event_tx`) — `None`.
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
            tze_hud_projection::hub::PortalHub::default(),
            Arc::clone(&clock),
        );
        let shared_state = Arc::new(TokioMutex::new(SharedStateBuilder::build(
            Arc::clone(&safe_mode_atomic),
            Arc::clone(&active_tab_mirror),
            clock,
        )));

        let (frame_ready_tx, frame_ready_rx) = crate::channels::frame_ready_channel();
        // Input-capture / paste channels: the drain path never reads these, but
        // the fields require a receiver. Drop the senders — a disconnected
        // receiver is harmless here.
        let (_input_capture_tx, input_capture_rx) = tokio::sync::mpsc::unbounded_channel();

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
            pairing: None,
            control_panel: None,
            input_ring: Arc::new(StdMutex::new(VecDeque::new())),
            pending_input_latency: Arc::new(StdMutex::new(VecDeque::new())),
            frame_ready_rx,
            deferred_present: false,
            frame_ready_tx: Some(frame_ready_tx),
            frame_presented_tx: None,
            degradation_notices: None,
            compositor: None,
            window_surface: None,
            input_processor: InputProcessor::new(),
            input_capture_rx,
            pending_input_capture_commands: VecDeque::new(),
            focus_manager: FocusManager::new(),
            keyboard_processor: KeyboardProcessor::new(),
            telemetry: tze_hud_telemetry::TelemetryCollector::new(),
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
            modifiers: winit::keyboard::ModifiersState::empty(),
            primary_monitor: None,
            secondaries: Vec::new(),
            display_targets: Default::default(),
            displays_dirty: Default::default(),
            surface_factory: None,
            zone_displays: Default::default(),
            cursor_window: None,
            secondary_recreates: Default::default(),
            overlay_refits: Default::default(),
            press_window: None,
            zone_press: None,
            notification_link_opener: Default::default(),
            context_menu_press: None,
            context_menu_open: false,
            context_menu_dismiss_pending: false,
            notification_keydowns: Default::default(),
            primary_drag_gesture: None,
            global_tokens: HashMap::new(),
            appearance: None,
            tokens_seen: 0,
            _config_watch: None,
            element_repositioned_tx: None,
            input_event_tx: None,
            composer_pointer_drag_anchor: None,
            portal_resize_states: HashMap::new(),
            consumed_portal_resize_keydowns: HashSet::new(),
            consumed_shell_shortcut_keydowns: HashSet::new(),
            panel_input: crate::windowed::panel_input::PanelInputState::default(),
            keyboard_activation_nodes: HashMap::new(),
            consumed_command_keydowns: HashSet::new(),
            local_composer_state: Arc::new(StdMutex::new(None)),
            focus_ring_owner_state: Arc::new(StdMutex::new(None)),
            resize_grip_hover_state: Arc::new(StdMutex::new(None)),
            tile_close_hover_state: Arc::new(StdMutex::new(None)),
            cursor_left_window: false,
            composer_visual_layout: Arc::new(StdMutex::new(None)),
            portal_projection_driver: crate::portal::PortalDriver::new(portals),
            capture_inbox: crate::operator::screenshot::CaptureInbox::detached(),
            pending_keyboard_events: VecDeque::new(),
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
            element_store_path: None,
            safe_mode_atomic,
            active_tab_mirror,
            token_store: tze_hud_protocol::token::TokenStore::new(),
            input_capture_tx: None,
            input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
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
    mcp_addr: Option<SocketAddr>,
    grpc_addr: Option<SocketAddr>,
    clipboard: Arc<std::sync::Mutex<Vec<String>>>,
    /// What the panel's System and Network rows did outside the HUD
    /// (`open <path>`, `firewall <action> <ports>`, `autostart <on>`),
    /// recorded instead of done.
    effects: Arc<std::sync::Mutex<Vec<String>>>,
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
        let global_tokens = match &raw {
            Some(raw) => {
                let config_parent = cfg
                    .config_file_path
                    .as_deref()
                    .and_then(|p| std::path::Path::new(p).parent());
                crate::scene_startup::run_scene_startup(raw, config_parent, &mut scene)
                    .global_tokens
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
            tze_hud_projection::hub::PortalHub::default(),
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
        state.pairing = pairing.clone();
        let render_wake = state.wake.render_notifier();
        state.live = Arc::new(
            crate::launch::LiveSettings::new(cfg.target_fps, cfg.debug_zones, render_wake.clone())
                .with_answer_window(cfg.answer_window_ms),
        );
        let mcp_config = crate::mcp::McpServerConfig {
            appearance: None,
            publication_defaults: state.live.publication_defaults.clone(),
            widget_transition_ms: tze_hud_config::tokens::resolve_motion_duration_ms(
                &global_tokens,
                "motion.state.ms",
            ),
            bind_addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
            late_tailnet_port: None,
            agents: Arc::clone(&cfg.agents),
            presents: None,
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
                    runtime_context.operational_envelope.max_leased_tiles,
                    runtime_context
                        .operational_envelope
                        .max_agent_leased_texture_bytes,
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
        ));
        state.appearance = Some(Arc::clone(&appearance));
        let clipboard: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let effects: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let deps = crate::shell::control_panel::ControlPanelDeps {
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

        let driver = crate::portal::PortalDriver::new(portals);
        state.portal_projection_driver = driver;
        state.global_tokens = global_tokens;
        state.runtime_context = runtime_context;
        state.config = cfg;
        Ok(HeadlessEventLoopHarness {
            app: WinitApp { state },
            mcp_addr: Some(mcp_addr),
            grpc_addr: Some(grpc_addr),
            clipboard,
            effects,
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
        self.app
            .state
            .pairing
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

    /// Number of tiles showing the disconnection badge (their agent's session
    /// dropped and the lease is in its grace period).
    pub fn badged_tile_count(&self) -> usize {
        self.scene()
            .tiles
            .values()
            .filter(|tile| {
                tile.visual_hint == tze_hud_scene::lease::TileVisualHint::DisconnectionBadge
            })
            .count()
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

    /// The text every tile on screen shows.
    pub fn tile_texts(&self) -> Vec<String> {
        let scene = self.scene();
        scene
            .tiles
            .values()
            .filter_map(|tile| tile.root_node)
            .filter_map(|id| scene.nodes.get(&id))
            .filter_map(|node| match &node.data {
                NodeData::TextMarkdown(text) => Some(text.content.clone()),
                _ => None,
            })
            .collect()
    }

    /// Run one GPU-free event-loop turn: the main-thread settle sequence from
    /// `about_to_wait` (composer flush, deferred keys, MCP portal ops, portal
    /// projection drain), then the compositor's Stage 4 timed-content sweep
    /// and the notification hit-region refresh its post-render pass does.
    ///
    /// Returns `false` when a lock was busy and part of the turn was deferred;
    /// call again.
    pub fn tick(&mut self) -> bool {
        self.app.sync_design_tokens();
        let settled = !self.app.settle_scene_work().is_deferred();
        let (width, height) = (
            self.app.state.config.window.width as f32,
            self.app.state.config.window.height as f32,
        );
        let Ok(state) = self.app.state.shared_state.try_lock() else {
            return false;
        };
        let Ok(mut scene) = state.scene.try_lock() else {
            return false;
        };
        crate::pipeline::sweep_timed_scene_state(&mut scene);
        tze_hud_compositor::renderer::hit_regions::populate_notification_hit_regions(
            &mut scene,
            width,
            height,
            &std::collections::HashMap::new(),
        );
        // The compositor hands terminal lease transitions to the owning
        // sessions; removal is immediate without agent cooperation.
        settled
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
                self.app.panel_key_input(physical, &logical, true),
                "the panel is open"
            );
            self.app.panel_key_input(physical, &logical, false);
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
            self.app.panel_key_input(physical, &logical, true),
            "the panel is open"
        );
        self.app.panel_key_input(physical, &logical, false);
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

    /// Display-space center of the first composer input region on screen
    /// (a portal armed with `expects_reply`), where a user would click to type.
    pub fn composer_center(&self) -> Option<(f32, f32)> {
        let state = self.app.state.shared_state.try_lock().ok()?;
        let scene = state.scene.try_lock().ok()?;
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
        let state = self.app.state.shared_state.try_lock().ok()?;
        let scene = state.scene.try_lock().ok()?;
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

    /// Number of tiles on screen (portal surfaces included).
    pub fn tile_count(&self) -> usize {
        self.scene().tiles.len()
    }

    /// Build a harness around an inert-but-real `WinitApp`.
    #[cfg(test)]
    pub(super) fn new() -> Self {
        HeadlessEventLoopHarness {
            app: WinitApp {
                state: WindowedRuntimeState::new_headless(),
            },
            mcp_addr: None,
            grpc_addr: None,
            clipboard: Arc::default(),
            effects: Arc::default(),
        }
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
    use tze_hud_input::{KeyboardModifiers, RawCharacterEvent, RawKeyDownEvent, RawKeyUpEvent};
    use tze_hud_protocol::proto::input_envelope::Event as ProtoInputEvent;
    use tze_hud_protocol::proto::{CommandAction, CommandSource};
    use tze_hud_scene::MonoUs;

    /// Real frame builds and runtime accounting; only offscreen readback and
    /// swapchain acquire/submit/present are synthetic. GPU pixels and PNG have
    /// their own dedicated compositor/operator gates.
    struct CaptureRenderHarness {
        events: HeadlessEventLoopHarness,
        compositor: tze_hud_compositor::Compositor,
        frames: super::super::frame_work::WindowedFrameWork,
        pending_texture: bool,
    }

    impl CaptureRenderHarness {
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
                (320, 200),
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
            .maybe_present_frame_with_secondaries(|| None, |_| (false, vec![secondary]));
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
            .maybe_present_frame_with_secondaries(|| None, |_| (true, vec![]));
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
                (true, vec![])
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
                use tze_hud_scene::mutation::{MutationBatch, SceneMutation};
                use tze_hud_scene::types::{ZoneContent, ZonePublishToken, ZoneRegistry};
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
                    let id = SceneId::new();
                    ids.push(id);
                    scene.schedule_batch(
                        due,
                        MutationBatch {
                            batch_id: id,
                            agent_namespace: "delayed".into(),
                            lease_id: Some(lease),
                            timing_hints: None,
                            mutations: vec![SceneMutation::PublishToZone {
                                zone_name: "subtitle".into(),
                                content: ZoneContent::StreamText("capture".into()),
                                publish_token: ZonePublishToken { token: Vec::new() },
                                merge_key: None,
                                expires_at_wall_us: Some(due + 20_000_000),
                                content_classification: None,
                                breakpoints: Vec::new(),
                                held: false,
                            }],
                        },
                    );
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
                                    label: "Open".into(),
                                    callback_id: None,
                                    url: Some("https://example.com/frame".into()),
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

    fn ctrl_key_up(key_code: &str, key: &str, shift: bool, ts: u64) -> PendingKeyboardEvent {
        PendingKeyboardEvent::KeyUp(RawKeyUpEvent {
            key_code: key_code.to_string(),
            key: key.to_string(),
            modifiers: KeyboardModifiers {
                ctrl: true,
                shift,
                ..KeyboardModifiers::NONE
            },
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
                label: label.to_string(),
                callback_id: Some(id.to_string()),
                url: None,
            };
            scene
                .publish_to_zone(
                    "notification-area",
                    ZoneContent::Notification(NotificationPayload {
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
                tze_hud_scene::graph::PendingActionKind::Dismiss => {
                    panic!("expected a publisher callback, received a human X")
                }
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
    /// the click: one press on Approve queues exactly one `approve` (hud-z21e9).
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
    /// (the geometry epoch) while an unchanged move does not (hud-paslt).
    #[test]
    fn action_button_hover_and_press_are_local_and_frame_only_on_change() {
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
                let (tx, mut repositioned) = tokio::sync::broadcast::channel(4);
                harness.app.state.element_repositioned_tx = Some(tx);
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
                let ring = harness.app.state.input_ring.lock().unwrap().len();
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
                assert_eq!(
                    harness.app.state.input_ring.lock().unwrap().len(),
                    ring,
                    "cancel creates no synthetic input"
                );
                if held_terminal {
                    assert!(harness.app.settle_scene_work().is_deferred());
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
                assert!(repositioned.try_recv().is_err());
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
        assert!(!receipts.permit(window), "ambiguity disables until restart");
        assert!(receipts.install(window).is_none(), "no automatic rearm");

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
        let mut receipts = ChromeCaptureLedger::default();
        for _ in 0..64 {
            assert!(receipts.install(window).is_some());
        }
        assert!(
            receipts.install(window).is_none(),
            "bounded installation admission"
        );
    }

    /// One click on Approve queues exactly one `approve` and dismisses the
    /// notification, so the click has an immediate visible result and a
    /// second click has nothing to land on (hud-paslt).
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
                    label: "Docs".into(),
                    callback_id: None,
                    url: Some(urls[0].into()),
                };
                payload.actions[1] = NotificationAction {
                    label: "More".into(),
                    callback_id: None,
                    url: Some(urls[1].into()),
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
            for elapsed in [0, 1999] {
                let before = harness.app.state.notification_link_opener.workers.len();
                clock.advance(elapsed);
                click(&mut harness, points[0]);
                assert_eq!(
                    harness.app.state.notification_link_opener.workers.len(),
                    before
                );
                assert!(rx.try_recv().is_err());
            }
            clock.advance(1);
            click(&mut harness, points[0]);
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().0, url);
            click(&mut harness, points[1]);
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().0, urls[1]);
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
            clock.advance(2000);
            click(&mut harness, points[0]);
            rx.recv_timeout(Duration::from_secs(2)).unwrap();
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
            panel.set_geometry(1, || geometry.clone());
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
    /// dismiss button, queues no action the user did not press (hud-z21e9).
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
    /// and dismisses the answered notification (hud-paslt).
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
                        text: "Ship v2?".to_string(),
                        icon: String::new(),
                        urgency: 1,
                        ttl_ms: None,
                        title: "Deploy".to_string(),
                        actions: vec![NotificationAction {
                            label: "Ship".to_string(),
                            callback_id: Some("ship".to_string()),
                            url: None,
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
                        text: "Keep this notification".into(),
                        icon: String::new(),
                        urgency: 1,
                        ttl_ms: None,
                        title: "Control".into(),
                        actions: vec![NotificationAction {
                            label: "Keep".into(),
                            callback_id: Some("keep".into()),
                            url: None,
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
    }

    /// Install one ordinary focused button (not a portal composer/control) and
    /// return its ids plus a receiver for the runtime's real input-event channel.
    fn install_button(
        harness: &mut HeadlessEventLoopHarness,
        initially_focused: bool,
    ) -> (
        SceneId,
        SceneId,
        tze_hud_protocol::session_server::InputEventReceiver,
    ) {
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

        let tx = tze_hud_protocol::session_server::InputEventSender::new(16);
        let rx = tx.subscribe_all();
        harness.app.state.input_event_tx = Some(tx);
        (tile_id, node_id, rx)
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
                let (tile_id, node_id, _rx) = install_button(&mut harness, true);
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
                assert!(!harness.tick());
                assert_retained_focus(&harness);
                drop(scene_guard);
                drop(shared_guard);
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

    fn received_events(
        rx: &mut tze_hud_protocol::session_server::InputEventReceiver,
    ) -> Vec<(String, ProtoInputEvent)> {
        let mut events = Vec::new();
        while let Ok((namespace, batch)) = rx.try_recv() {
            events.extend(
                batch
                    .events
                    .into_iter()
                    .filter_map(|envelope| envelope.event)
                    .map(|event| (namespace.clone(), event)),
            );
        }
        events
    }

    /// Install two scrollable tiles with keyboard focus on the first while the
    /// cursor is over the second. This is the adversarial setup for keyboard
    /// scroll ownership: focus and pointer location deliberately disagree.
    fn install_conflicting_focus_and_pointer_scroll_tiles(
        harness: &mut HeadlessEventLoopHarness,
    ) -> (
        SceneId,
        SceneId,
        tze_hud_protocol::session_server::InputEventReceiver,
    ) {
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

        let tx = tze_hud_protocol::session_server::InputEventSender::new(16);
        let rx = tx.subscribe_all();
        harness.app.state.input_event_tx = Some(tx);
        (focused_tile, pointer_tile, rx)
    }

    /// RFC 0007 §2.3 / system-shell "Tab Keyboard Shortcuts": the runtime
    /// consumes Ctrl+Tab before agent routing and changes the authoritative
    /// scene tab locally. Drive the real event-loop drain so this proves the
    /// production precedence seam, not only `shell::handle_shortcut` in
    /// isolation.
    #[test]
    fn ctrl_tab_switches_scene_and_key_down_never_reaches_agent() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, _node_id, mut rx) = install_button(&mut harness, true);
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
        assert!(
            received_events(&mut rx).is_empty(),
            "a shell-reserved KeyDown must never reach an agent"
        );
    }

    /// Runtime tab topology can grow after chrome labels are configured. The
    /// shell reconciliation must retain labels for existing chrome slots and
    /// synthesize a neutral label only for the newly-discovered slot.
    #[test]
    fn ctrl_tab_preserves_configured_chrome_label_when_scene_adds_tab() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, _node_id, mut rx) = install_button(&mut harness, true);
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
        assert!(
            received_events(&mut rx).is_empty(),
            "the shell-owned tab shortcut must remain agent-inaccessible"
        );
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
                let (_tile_id, _node_id, mut rx) = install_button(&mut harness, true);
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
                assert!(
                    received_events(&mut rx).is_empty(),
                    "shell-reserved {down_key} KeyDown must never reach an agent"
                );
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
                    received_events(&mut rx).is_empty(),
                    "matching {up_key} KeyUp must never reach an agent"
                );
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

    /// The shell intercept is an exact reserved-set match, not a blanket Ctrl
    /// filter. Ordinary focused-agent shortcuts keep their raw down/up route.
    #[test]
    fn non_reserved_ctrl_key_sequence_still_reaches_focused_agent() {
        for (key_code, key, shift, alt) in [
            ("KeyA", "a", false, false),
            ("KeyM", "m", false, false),
            ("KeyM", "M", true, true),
        ] {
            let mut harness = HeadlessEventLoopHarness::new();
            let (_tile_id, _node_id, mut rx) = install_button(&mut harness, true);
            let mut down = ctrl_key_down(key_code, key, shift, 1_000);
            let mut up = ctrl_key_up(key_code, key, shift, 1_100);
            if let PendingKeyboardEvent::KeyDown(raw) = &mut down {
                raw.modifiers.alt = alt;
            }
            if let PendingKeyboardEvent::KeyUp(raw) = &mut up {
                raw.modifiers.alt = alt;
            }
            harness.enqueue(down);
            harness.enqueue(up);
            harness.drain();
            assert_eq!(harness.pending_len(), 0);

            let events = received_events(&mut rx);
            assert!(
                events
                    .iter()
                    .any(|(_, event)| matches!(event, ProtoInputEvent::KeyDown(_))),
                "non-reserved {key} KeyDown must retain raw agent routing; got {events:?}"
            );
            assert!(
                events
                    .iter()
                    .any(|(_, event)| matches!(event, ProtoInputEvent::KeyUp(_))),
                "non-reserved {key} KeyUp must retain raw agent routing; got {events:?}"
            );
        }
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
            let tx = tze_hud_protocol::session_server::InputEventSender::new(128);
            let mut rx = tx.subscribe_all();
            harness.app.state.input_event_tx = Some(tx);
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
            let _ = received_events(&mut rx);
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
            assert!(received_events(&mut rx).iter().all(|(_, event)| !matches!(
                event,
                ProtoInputEvent::KeyDown(_)
                    | ProtoInputEvent::KeyUp(_)
                    | ProtoInputEvent::Character(_)
                    | ProtoInputEvent::CommandInput(_)
            )));
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
                    let tx = tze_hud_protocol::session_server::InputEventSender::new(128);
                    let mut rx = tx.subscribe_all();
                    harness.app.state.input_event_tx = Some(tx);
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
                    assert!(
                        received_events(&mut rx)
                            .iter()
                            .all(|(_, event)| !matches!(event, ProtoInputEvent::KeyUp(_))),
                        "focus loss preserves the consumed physical Tab release"
                    );
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

    /// RFC 0004 §10 production proof: keyboard is a concrete pointer-free
    /// command source, not merely a library/test fixture. Drive the real pending
    /// keyboard drain and require ACTIVATE to emerge on the runtime broadcast.
    #[test]
    fn real_keyboard_drain_dispatches_activate_command_and_local_feedback() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (tile_id, node_id, mut rx) = install_button(&mut harness, true);

        harness.enqueue(key_down("Enter", "Enter", 1_000));
        harness.drain();

        let events = received_events(&mut rx);
        let command = events.iter().find_map(|(namespace, event)| match event {
            ProtoInputEvent::CommandInput(command) => Some((namespace, command)),
            _ => None,
        });
        let (namespace, command) = command.unwrap_or_else(|| {
            panic!("production keyboard drain must emit CommandInputEvent; got {events:?}")
        });
        assert_eq!(namespace, "command-agent");
        assert_eq!(command.tile_id, tile_id.as_uuid().as_bytes());
        assert_eq!(command.node_id, node_id.as_uuid().as_bytes());
        assert_eq!(command.interaction_id, "primary-action");
        assert_eq!(command.action, CommandAction::Activate as i32);
        assert_eq!(command.source, CommandSource::Keyboard as i32);

        let shared = harness.app.state.shared_state.blocking_lock();
        let scene = shared.scene.blocking_lock();
        assert!(
            scene
                .hit_region_states
                .get(&node_id)
                .is_some_and(|state| state.pressed),
            "ACTIVATE must set local pressed feedback before agent delivery"
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

    /// RFC 0004 §5.7/§10.3: PageDown is focus-routed command input, not a
    /// pointer-position scroll. The focused tile must receive both local scroll
    /// feedback and the command even when the cursor rests over another tile.
    #[test]
    fn page_down_scrolls_focused_tile_not_tile_under_pointer() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (focused_tile, pointer_tile, mut rx) =
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

        let events = received_events(&mut rx);
        assert_eq!(
            events.len(),
            2,
            "PageDown must deliver one command then one resulting offset; got {events:?}"
        );
        let (command_namespace, ProtoInputEvent::CommandInput(command)) = &events[0] else {
            panic!("PageDown must deliver CommandInputEvent first; got {events:?}")
        };
        assert_eq!(command_namespace, "scroll-agent");
        assert_eq!(command.tile_id, focused_tile.as_uuid().as_bytes());
        assert_eq!(command.action, CommandAction::ScrollDown as i32);
        assert_eq!(command.source, CommandSource::Keyboard as i32);
        let (scroll_namespace, ProtoInputEvent::ScrollOffsetChanged(scroll)) = &events[1] else {
            panic!(
                "PageDown must deliver resulting ScrollOffsetChangedEvent second; got {events:?}"
            )
        };
        assert_eq!(scroll_namespace, "scroll-agent");
        assert_eq!(scroll.tile_id, focused_tile.as_uuid().as_bytes());
        assert_eq!(scroll.offset_y, tze_hud_input::KEYBOARD_PAGE_SCROLL_PX);
    }

    /// PageUp shares the focus-routed ownership contract with PageDown. Seed
    /// both tiles away from origin so scrolling the wrong one is observable.
    #[test]
    fn page_up_scrolls_focused_tile_not_tile_under_pointer() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (focused_tile, pointer_tile, mut rx) =
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
        let _ = received_events(&mut rx);

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

        let events = received_events(&mut rx);
        assert!(
            events.iter().any(|(_, event)| matches!(
                event,
                ProtoInputEvent::CommandInput(command)
                    if command.tile_id == focused_tile.as_uuid().as_bytes()
                        && command.action == CommandAction::ScrollUp as i32
            )),
            "PageUp must retain focused SCROLL_UP command delivery; got {events:?}"
        );
    }

    /// The keyboard correction must not alter wheel ownership: wheel input is
    /// still pointer-routed even when keyboard focus belongs to another tile.
    #[test]
    fn wheel_scroll_still_targets_tile_under_pointer_not_focused_tile() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (focused_tile, pointer_tile, _rx) =
            install_conflicting_focus_and_pointer_scroll_tiles(&mut harness);

        harness.app.enqueue_scroll_event(0.0, 40.0);

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

    /// ArrowDown remains an abstract command binding at tile focus, but this
    /// PageUp/PageDown correction must not invent a page-sized local side
    /// effect for that distinct key.
    #[test]
    fn arrow_down_command_does_not_apply_page_scroll_side_effect() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (focused_tile, pointer_tile, mut rx) =
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

        let events = received_events(&mut rx);
        assert!(
            events.iter().any(|(_, event)| matches!(
                event,
                ProtoInputEvent::CommandInput(command)
                    if command.tile_id == focused_tile.as_uuid().as_bytes()
                        && command.action == CommandAction::ScrollDown as i32
            )),
            "ArrowDown must retain its focused SCROLL_DOWN command; got {events:?}"
        );
        assert!(
            events
                .iter()
                .all(|(_, event)| !matches!(event, ProtoInputEvent::ScrollOffsetChanged(_))),
            "ArrowDown must not emit a page-scroll offset event; got {events:?}"
        );
    }

    /// A command binding replaces the raw key sequence, not just the
    /// key-down. A focused agent must never receive a raw release without the
    /// matching raw press after the Context Menu key was translated to CONTEXT.
    #[test]
    fn command_binding_swallows_matching_raw_key_up() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, _node_id, mut rx) = install_button(&mut harness, true);

        harness.enqueue(key_down("ContextMenu", "ContextMenu", 1_000));
        harness.enqueue(key_up("ContextMenu", "ContextMenu", 1_100));
        harness.drain();

        let events = received_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|(_, event)| matches!(event, ProtoInputEvent::CommandInput(command) if command.action == CommandAction::Context as i32)),
            "ContextMenu must be translated to the CONTEXT command; got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|(_, event)| matches!(event, ProtoInputEvent::KeyUp(_))),
            "a command binding must swallow its matching raw KeyUp; got {events:?}"
        );
    }

    /// Bare Tab already moved focus in production, but it skipped the abstract
    /// command pipeline. Require the real drain to deliver NAVIGATE_NEXT to the
    /// newly focused owner after local focus movement.
    #[test]
    fn real_keyboard_drain_dispatches_navigate_next_to_new_focus_owner() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, node_id, mut rx) = install_button(&mut harness, false);

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
            "Tab must move focus locally before command delivery"
        );
        let events = received_events(&mut rx);
        let command = events.iter().find_map(|(_, event)| match event {
            ProtoInputEvent::CommandInput(command) => Some(command),
            _ => None,
        });
        let command = command.unwrap_or_else(|| {
            panic!("Tab must emit NAVIGATE_NEXT CommandInputEvent; got {events:?}")
        });
        assert_eq!(command.action, CommandAction::NavigateNext as i32);
        assert_eq!(command.source, CommandSource::Keyboard as i32);
        assert_eq!(command.node_id, node_id.as_uuid().as_bytes());
    }

    /// Composer-less Escape retains the existing focus-recovery behavior while
    /// also delivering the RFC 0004 CANCEL command to the owner that had focus.
    #[test]
    fn real_keyboard_drain_dispatches_cancel_before_focus_owner_is_lost() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, node_id, mut rx) = install_button(&mut harness, true);

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

        let events = received_events(&mut rx);
        let command = events.iter().find_map(|(_, event)| match event {
            ProtoInputEvent::CommandInput(command) => Some(command),
            _ => None,
        });
        let command = command.unwrap_or_else(|| {
            panic!("Escape must emit CANCEL before its focus target is lost; got {events:?}")
        });
        assert_eq!(command.action, CommandAction::Cancel as i32);
        assert_eq!(command.source, CommandSource::Keyboard as i32);
        assert_eq!(command.node_id, node_id.as_uuid().as_bytes());
    }

    /// hud-nu0ea headline: the keyboard-drain full path runs end-to-end through
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
    /// `about_to_wait`, so the exit must also stop that wait (hud-gr6jp).
    #[test]
    fn shutdown_leaves_the_event_loop_without_waiting_for_a_message() {
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

        let harness = HeadlessEventLoopHarness::new();
        let idle = Recorder::default();
        assert!(!harness.app.leave_if_shutdown(&idle));
        assert!(
            idle.0.borrow().is_empty(),
            "no shutdown: the loop is untouched"
        );

        harness
            .app
            .state
            .shutdown
            .trigger(crate::threads::ShutdownReason::Clean);
        let quitting = Recorder::default();
        assert!(harness.app.leave_if_shutdown(&quitting));
        assert_eq!(*quitting.0.borrow(), [Some(ControlFlow::Poll), None]);
    }
}
