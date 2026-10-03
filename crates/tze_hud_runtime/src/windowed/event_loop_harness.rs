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
//! fields are left `None`.
//!
//! ## Networked mode (POC acceptance)
//!
//! [`HeadlessEventLoopHarness::with_network`] boots the same state from a real
//! config with the production MCP HTTP server on a loopback ephemeral port and
//! the MCP → event-loop portal-op channel, all reading one injected clock.
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
use tze_hud_scene::types::ZoneInteractionKind;
use tze_hud_scene::{Clock, MonoUs, NodeData, SceneId};
#[cfg(test)]
use tze_hud_scene::{Node, Rect, types::HitRegionNode};

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
    /// - the broadcast/op channels (`element_repositioned_tx`, `input_event_tx`,
    ///   `portal_op_rx`, `safe_mode_exit_tx`) — `None`.
    ///
    /// The `safe_mode_atomic` and `active_tab_mirror` `Arc`s are shared between
    /// the state and its embedded [`SharedState`] exactly as production does, so
    /// the lock-free keyboard-dispatch reads observe the same values a real run
    /// would.
    pub(super) fn new_headless() -> Self {
        use std::collections::{HashMap, HashSet, VecDeque};
        use std::sync::Mutex as StdMutex;
        use std::sync::atomic::AtomicBool;

        use tokio::sync::Mutex as TokioMutex;

        let safe_mode_atomic = Arc::new(AtomicBool::new(false));
        let active_tab_mirror: Arc<StdMutex<Option<SceneId>>> = Arc::new(StdMutex::new(None));

        let shared_state = Arc::new(TokioMutex::new(SharedStateBuilder::build(
            Arc::clone(&safe_mode_atomic),
            Arc::clone(&active_tab_mirror),
        )));

        let (frame_ready_tx, frame_ready_rx) = crate::channels::frame_ready_channel();
        // Input-capture / paste channels: the drain path never reads these, but
        // the fields require a receiver. Drop the senders — a disconnected
        // receiver is harmless here.
        let (_input_capture_tx, input_capture_rx) = tokio::sync::mpsc::unbounded_channel();

        WindowedRuntimeState {
            wake: super::wake::WindowedWake::disconnected(),
            scheduled_main_deadline: None,
            config: super::WindowedConfig::default(),
            compositor_handle: None,
            network_rt: None,
            network_handles: Vec::new(),
            runtime_context: Arc::new(crate::runtime_context::RuntimeContext::headless_default()),
            _runtime_widget_store: None,
            shared_state,
            safe_mode_atomic,
            active_tab_mirror,
            safe_mode_exit_tx: None,
            chrome_state: Arc::new(std::sync::RwLock::new(crate::shell::ChromeState::new())),
            input_ring: Arc::new(StdMutex::new(VecDeque::new())),
            pending_input_latency: Arc::new(StdMutex::new(VecDeque::new())),
            frame_ready_rx,
            frame_ready_tx: Some(frame_ready_tx),
            frame_presented_tx: None,
            degradation_notices: None,
            lease_expirations: None,
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
            benchmark_failed: Arc::new(AtomicBool::new(false)),
            terminal_surface_recovery_failed: Arc::new(AtomicBool::new(false)),
            quiescent_efficiency: None,
            cursor_x: 0.0,
            cursor_y: 0.0,
            left_button_down: false,
            cursor_tracker: tze_hud_input::CursorIconTracker::new(),
            window: None,
            effective_mode: crate::window::WindowMode::Fullscreen,
            hit_regions: Vec::new(),
            static_hit_regions: Vec::new(),
            widget_hover_trackers: HashMap::new(),
            pending_mode_switch: None,
            pending_widget_svgs: Vec::new(),
            modifiers: winit::keyboard::ModifiersState::empty(),
            current_monitor_index: 0,
            global_tokens: HashMap::new(),
            element_repositioned_tx: None,
            input_event_tx: None,
            pending_blur_delivery_context: None,
            composer_pointer_drag_anchor: None,
            portal_resize_states: HashMap::new(),
            consumed_portal_resize_keydowns: HashSet::new(),
            consumed_shell_shortcut_keydowns: HashSet::new(),
            keyboard_activation_nodes: HashMap::new(),
            consumed_command_keydowns: HashSet::new(),
            local_composer_state: Arc::new(StdMutex::new(None)),
            viewer_echo_queue: Arc::new(StdMutex::new(Vec::new())),
            input_history_seed_states: HashMap::new(),
            focus_ring_owner_state: Arc::new(StdMutex::new(None)),
            resize_grip_hover_state: Arc::new(StdMutex::new(None)),
            composer_visual_layout: Arc::new(StdMutex::new(None)),
            portal_projection_driver: crate::portal_projection_driver::InProcessPortalDriver::new(),
            portal_op_rx: None,
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
    ) -> tze_hud_protocol::session::SharedState {
        use tokio::sync::Mutex as TokioMutex;
        tze_hud_protocol::session::SharedState {
            scene: Arc::new(TokioMutex::new(SceneGraph::new(1920.0, 1080.0))),
            sessions: tze_hud_protocol::session::SessionRegistry::new(),
            resource_store: tze_hud_resource::ResourceStore::new(
                tze_hud_resource::ResourceStoreConfig::default(),
            ),
            widget_asset_store: tze_hud_protocol::session::WidgetAssetStore::default(),
            runtime_widget_store: None,
            element_store: tze_hud_scene::element_store::ElementStore::default(),
            element_store_path: None,
            safe_mode_atomic,
            active_tab_mirror,
            token_store: tze_hud_protocol::token::TokenStore::new(),
            freeze_active: false,
            input_capture_tx: None,
            input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            tile_placement: Default::default(),
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
        let runtime_context = super::build_runtime_context(&cfg);
        let mut state = WindowedRuntimeState::new_headless();

        let mut scene = SceneGraph::new_with_clock(
            cfg.window.width as f32,
            cfg.window.height as f32,
            Arc::clone(&clock),
        );
        let global_tokens = match cfg.config_toml.as_deref() {
            Some(toml_src) => {
                let raw: tze_hud_config::raw::RawConfig = toml::from_str(toml_src)?;
                let config_parent = cfg
                    .config_file_path
                    .as_deref()
                    .and_then(|p| std::path::Path::new(p).parent());
                crate::scene_startup::run_scene_startup(&raw, config_parent, &mut scene)
                    .global_tokens
            }
            None => {
                scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
                std::collections::HashMap::new()
            }
        };
        let scene_handle = {
            let mut shared = state.shared_state.lock().await;
            shared.tile_placement = tze_hud_config::tile_placement_from_tokens(&global_tokens);
            *shared.scene.lock().await = scene;
            Arc::clone(&shared.scene)
        };

        let (portal_op_tx, portal_op_rx) = tokio::sync::mpsc::unbounded_channel();
        let mcp_config = crate::mcp::McpServerConfig {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            agents: Arc::clone(&cfg.agents),
        };
        let (_mcp_task, mcp_addr) = crate::mcp::start_mcp_http_server(
            scene_handle,
            mcp_config,
            state.shutdown.clone(),
            Some(portal_op_tx),
        )
        .await?;

        let mut driver = super::build_portal_projection_driver(&cfg)?;
        driver.set_clock(clock);
        state.portal_projection_driver = driver;
        state.portal_op_rx = Some(portal_op_rx);
        state.global_tokens = global_tokens;
        state.runtime_context = runtime_context;
        state.config = cfg;
        Ok(HeadlessEventLoopHarness {
            app: WinitApp { state },
            mcp_addr: Some(mcp_addr),
        })
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

    /// Run one GPU-free event-loop turn: the main-thread settle sequence from
    /// `about_to_wait` (composer flush, deferred keys, MCP portal ops, portal
    /// projection drain), then the compositor's Stage 4 timed-content sweep
    /// and the notification hit-region refresh its post-render pass does.
    ///
    /// Returns `false` when a lock was busy and part of the turn was deferred;
    /// call again.
    pub fn tick(&mut self) -> bool {
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
        let _ = crate::pipeline::sweep_timed_scene_state(&mut scene);
        tze_hud_compositor::renderer::hit_regions::populate_notification_hit_regions(
            &mut scene, width, height,
        );
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
        let state = self.app.state.shared_state.try_lock().ok()?;
        let scene = state.scene.try_lock().ok()?;
        scene
            .overlay
            .zone_hit_regions
            .iter()
            .find(|region| {
                matches!(&region.kind, ZoneInteractionKind::Action { callback_id: id } if id == callback_id)
            })
            .map(|region| {
                (
                    region.bounds.x + region.bounds.width / 2.0,
                    region.bounds.y + region.bounds.height / 2.0,
                )
            })
    }

    /// Number of publications currently shown in zone `zone`.
    pub fn zone_publication_count(&self, zone: &str) -> usize {
        let state = self.app.state.shared_state.try_lock().expect(BUSY);
        let scene = state.scene.try_lock().expect(BUSY);
        scene
            .zone_registry
            .active_publishes
            .get(zone)
            .map_or(0, Vec::len)
    }

    /// Number of tiles on screen (portal surfaces included).
    pub fn tile_count(&self) -> usize {
        let state = self.app.state.shared_state.try_lock().expect(BUSY);
        state.scene.try_lock().expect(BUSY).tiles.len()
    }

    /// Build a harness around an inert-but-real `WinitApp`.
    #[cfg(test)]
    pub(super) fn new() -> Self {
        HeadlessEventLoopHarness {
            app: WinitApp {
                state: WindowedRuntimeState::new_headless(),
            },
            mcp_addr: None,
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

/// Scene queries run between MCP calls, when nothing else holds the scene.
const BUSY: &str = "scene is locked by an in-flight call; query between calls";

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
        let mut scene = SceneGraph::new(1920.0, 1080.0);
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

        {
            let shared = harness.app.state.shared_state.blocking_lock();
            *shared.scene.blocking_lock() = scene;
        }
        *harness.app.state.active_tab_mirror.lock().unwrap() = Some(tab_id);

        let tx = tze_hud_protocol::session_server::InputEventSender::new(16);
        let rx = tx.subscribe_all();
        harness.app.state.input_event_tx = Some(tx);
        (tile_id, node_id, rx)
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
    fn matching_ctrl_tab_key_up_never_reaches_agent() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, _node_id, mut rx) = install_button(&mut harness, true);

        harness.enqueue(ctrl_key_down("Tab", "Tab", false, 1_000));
        harness.drain();
        let _ = received_events(&mut rx);

        harness.enqueue(PendingKeyboardEvent::KeyUp(RawKeyUpEvent {
            key_code: "Tab".to_string(),
            key: "Tab".to_string(),
            // The user may release Ctrl before Tab. The physical identity from
            // KeyDown, not release-time modifiers, must still own this KeyUp.
            modifiers: KeyboardModifiers::NONE,
            timestamp_mono_us: MonoUs(1_100),
        }));
        harness.drain();

        assert!(
            received_events(&mut rx).is_empty(),
            "a matching shell-reserved KeyUp must never reach an agent"
        );
    }

    /// Ctrl+Shift+F8/F9 are consumed one stage earlier by the real winit path,
    /// so the Stage-2 router can observe only their release. The complete
    /// reserved set remains shell-owned even for that release-only route.
    #[test]
    fn os_stage_shell_shortcut_release_never_reaches_agent() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, _node_id, mut rx) = install_button(&mut harness, true);

        harness.enqueue(ctrl_key_up("F9", "F9", true, 1_100));
        harness.drain();

        assert!(
            received_events(&mut rx).is_empty(),
            "the monitor-cycle KeyUp must remain shell-owned after Stage 1 consumed its KeyDown"
        );
    }

    /// The shell intercept is an exact reserved-set match, not a blanket Ctrl
    /// filter. Ordinary focused-agent shortcuts keep their raw down/up route.
    #[test]
    fn non_reserved_ctrl_key_sequence_still_reaches_focused_agent() {
        let mut harness = HeadlessEventLoopHarness::new();
        let (_tile_id, _node_id, mut rx) = install_button(&mut harness, true);

        harness.enqueue(ctrl_key_down("KeyA", "a", false, 1_000));
        harness.enqueue(ctrl_key_up("KeyA", "a", false, 1_100));
        harness.drain();

        let events = received_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|(_, event)| matches!(event, ProtoInputEvent::KeyDown(_))),
            "non-reserved Ctrl+A KeyDown must retain raw agent routing; got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|(_, event)| matches!(event, ProtoInputEvent::KeyUp(_))),
            "non-reserved Ctrl+A KeyUp must retain raw agent routing; got {events:?}"
        );
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
    /// `shared_state`. Holding that guard forces `ComposerDeliveryContextLookup::
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
}
