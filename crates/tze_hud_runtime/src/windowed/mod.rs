//! # windowed
//!
//! Windowed runtime — the production display path. Runs the full 8-stage frame
//! pipeline with a real `winit` window and `wgpu` swapchain.
//!
//! ## Architecture (spec §Thread Model, line 19)
//!
//! - **Main thread**: winit event loop, Stage 1 input drain, Stage 2 local
//!   feedback, surface.present() on `FrameReadySignal`.
//! - **Compositor thread**: Stages 3–7 (scene commit → GPU submit). Owns
//!   `wgpu::Device` and `wgpu::Queue` exclusively.
//! - **Network thread(s)**: Tokio runtime for gRPC and MCP.
//! - **Telemetry thread**: async structured emission.
//!
//! ## Window modes (spec §Window Modes, line 172)
//!
//! Two modes are supported, configured via `WindowedConfig::window.mode`:
//!
//! - **Fullscreen**: borderless fullscreen (`Fullscreen::Borderless`). The
//!   compositor owns the entire display with an opaque background. All input
//!   is captured (no passthrough).
//!
//! - **Overlay/HUD**: transparent, borderless, always-on-top window. Per-region
//!   input passthrough is implemented via `Window::set_cursor_hittest()`:
//!   - When the cursor is **inside** any active hit-region → `set_cursor_hittest(true)`
//!     (window captures the event).
//!   - When the cursor is **outside** all hit-regions → `set_cursor_hittest(false)`
//!     (event passes through to the desktop).
//!     This gives the same semantic as the XShape extension / wlr-layer-shell approach
//!     while using winit's cross-platform API.
//!
//! ## Runtime mode switching
//!
//! Mode switching is supported but disruptive (requires surface recreation, spec
//! line 173). The event loop stores a pending mode switch, tears down the existing
//! window and compositor, and re-initialises with the new mode on the next
//! `RedrawRequested` event (where the pending switch is detected before the frame
//! is presented).
//!
//! ## Main thread event loop
//!
//! The winit event loop runs on the main thread (OS requirement on macOS).
//! On each `WindowEvent::RedrawRequested`, the main thread:
//! 1. Drains pending `PointerEvent` / `KeyboardEvent` from the input channel.
//! 2. Checks `FrameReadySignal` (tokio::sync::watch) for a compositor-ready signal.
//! 3. Calls `surface.get_current_texture()` then `surface_texture.present()` if
//!    a frame is ready.
//!
//! Input events are forwarded to the compositor thread via `input_tx` (ring buffer).
//!
//! ## Input integration
//!
//! winit `WindowEvent` → `PointerEvent` or `KeyboardEvent`:
//! - `CursorMoved`  → `PointerEvent { kind: Move, x, y }`
//! - `MouseInput`   → `PointerEvent { kind: Down | Up, x, y }`
//! - `KeyboardInput`→ `KeyboardEvent { key_code, logical_key, modifiers, pressed }`
//!
//! Per spec §Stage 1 Input Drain (line 72): "MUST drain all pending OS input
//! events, attach hardware timestamps, produce InputEvent records, enqueue to
//! InputEvent channel."
//!
//! ## FrameReadySignal and surface.present()
//!
//! The compositor thread sends `true` on `FrameReadyTx` after GPU submit.
//! The main thread loop detects the change (via `watch::Receiver::has_changed`)
//! and calls `present_pending_frame()`.
//!
//! Per spec §Compositor Thread Ownership (line 46): "The main thread SHALL hold
//! the surface handle and be the only thread that calls surface.present()."
//!
//! ## Window resize
//!
//! On `WindowEvent::Resized`, the main thread calls `surface.reconfigure()`.
//! The compositor thread picks up the new size on the next `surface.size()` call.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use tokio::sync::Mutex;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Fullscreen, Window, WindowAttributes, WindowId, WindowLevel};

use crate::scene_startup::run_scene_startup;
use tze_hud_compositor::{
    Compositor, CompositorSurface, FocusRingOwnerHandle, LocalComposerStateHandle,
    PortalViewerEchoQueue, ResizeGripHoverHandle, TileCloseHoverHandle, WindowSurface,
};
use tze_hud_config::resolve_runtime_widget_asset_store;
use tze_hud_input::{
    CursorIconTracker, FocusManager, InputProcessor, KeyboardProcessor, PointerEventKind,
    PortalResizeState, RawCharacterEvent, RawKeyDownEvent, RawKeyUpEvent,
};
use tze_hud_protocol::session::SharedState;
use tze_hud_protocol::token::TokenStore;
use tze_hud_resource::{RuntimeWidgetStore, RuntimeWidgetStoreConfig};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::ZoneContent;
use tze_hud_telemetry::TelemetryCollector;

use crate::channels::{
    FrameReadyRx, FrameReadyTx, INPUT_EVENT_CAPACITY, InputEvent, InputEventKind,
    frame_ready_channel,
};
use crate::element_store::bootstrap_scene_element_store;
use crate::mcp::{McpServerConfig, start_mcp_http_server_with_render_wake};
use crate::pipeline::FramePipeline;
use crate::runtime_context::SharedRuntimeContext;
use crate::threads::{CompositorReady, NetworkRuntime, ShutdownToken, spawn_compositor_thread};
use crate::widget_hover::WidgetHoverTracker;
use crate::widget_runtime_registration::process_pending_widget_svgs;
use crate::window::{HitRegion, WindowMode};

/// RAII guard that raises the OS timer resolution to 1 ms for its lifetime.
///
/// On Windows the default scheduler timer granularity is ~15.6 ms, so the bare
/// `std::thread::sleep` used for compositor frame pacing overshoots its deadline
/// by up to a full timer tick. That quantization — not payload size — is the
/// dominant source of the live present-budget misses recorded in hud-ofe76
/// (present overhead p95 21 ms / max 56 ms against the 16.6 ms Windows lane
/// budget, with near-identical payloads ranging 0→56 ms). Holding
/// `timeBeginPeriod(1)` for the compositor thread's lifetime cuts sleep
/// granularity to ~1 ms, comfortably within budget; the period is released via
/// `timeEndPeriod(1)` when the guard drops on any loop-exit path.
///
/// No-op on non-Windows targets, whose sleep granularity is already sub-ms.
#[cfg(windows)]
struct FramePacingTimerGuard {
    active: bool,
}

#[cfg(not(windows))]
struct FramePacingTimerGuard;

impl FramePacingTimerGuard {
    fn acquire() -> Self {
        #[cfg(windows)]
        {
            // SAFETY: `timeBeginPeriod` requests a process-global timer
            // resolution and is paired with `timeEndPeriod(1)` on drop. 1 ms is
            // the standard media/compositor resolution. Returns TIMERR_NOERROR
            // (0) on success.
            let ok = unsafe { windows::Win32::Media::timeBeginPeriod(1) } == 0;
            if !ok {
                tracing::warn!(
                    "timeBeginPeriod(1) failed; frame pacing keeps the default \
                     ~15.6ms Windows timer resolution"
                );
            }
            Self { active: ok }
        }
        #[cfg(not(windows))]
        {
            Self
        }
    }
}

#[cfg(windows)]
impl Drop for FramePacingTimerGuard {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: paired with the `timeBeginPeriod(1)` in `acquire()`.
            unsafe {
                let _ = windows::Win32::Media::timeEndPeriod(1);
            }
        }
    }
}

mod config;
#[cfg(target_os = "windows")]
mod global_hotkey;
mod hittest;
mod input_dispatch;
mod keyboard;
mod lifecycle;
mod network;
mod portal;
mod safe_mode_toggle;
mod wake;
mod widgets;

#[cfg(test)]
mod test_support;

// Unit tests use the keyboard-drain surface; the networked mode is used only
// by `tests/integration` through the `test-harness` feature.
#[cfg(any(test, feature = "test-harness"))]
#[cfg_attr(not(feature = "test-harness"), allow(dead_code))]
mod event_loop_harness;

pub use self::config::{
    WindowedBenchmarkConfig, WindowedConfig, WindowedQuiescentEfficiencyConfig,
};
#[cfg(feature = "test-harness")]
pub use self::event_loop_harness::HeadlessEventLoopHarness;
use self::hittest::{refresh_interaction_hit_regions_after_render, sync_scene_display_area};
use self::input_dispatch::{
    enqueue_input, logical_key_to_str, nanoseconds_since_start, normalize_mouse_wheel_delta,
    physical_key_to_key_code_str, physical_key_to_u32, winit_mods_to_keyboard_modifiers,
};
use self::keyboard::{ComposerDeliveryContext, PendingKeyboardEvent};
use self::lifecycle::{
    BENCHMARK_NO_PROGRESS_TIMEOUT, PendingInputLatencySamples, WindowedBenchmarkRunState,
    WindowedQuiescentEfficiencyRunState, begin_os_mouse_capture, detect_monitor_size,
    drain_pending_input_latency, end_os_mouse_capture, focus_window_for_text_input,
    read_windows_clipboard_text, seed_windowed_benchmark_scene, update_surface_repaint_pending,
    windowed_frame_needs_render,
};
pub use self::network::render_attach_info;
use self::network::{
    build_runtime_context, render_startup_banner, start_network_services_with_render_wake,
};
use self::portal::PortalProjectionDrain;
use self::wake::{
    Deadline, RuntimeWakeEvent, WindowedWake, control_flow_for_deadlines, deadline_from_wall_us,
};
use crate::portal_projection_driver::PortalWakeDeadline;

fn publish_degradation_transition(
    controller: &crate::degradation::DegradationController,
    event: tze_hud_telemetry::DegradationEvent,
    notices: Option<&tze_hud_protocol::session_server::DegradationNoticeSender>,
) {
    tracing::warn!(
        previous_level = event.previous_level,
        new_level = event.new_level,
        direction = ?event.direction,
        p95_us = event.frame_time_p95_us,
        sample_count = event.sample_count,
        window_duration_us = event.window_duration_us,
        effective_cadence_hz = event.effective_cadence_hz,
        entry_threshold_us = event.entry_threshold_us,
        recovery_threshold_us = event.recovery_threshold_us,
        recovery_source = ?event.recovery_source,
        "runtime degradation transition"
    );
    if let Some(notices) = notices {
        let wall_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_micros() as u64)
            .unwrap_or(0);
        notices.publish_blocking(controller.protocol_notice(wall_us));
    }
}

/// Forward terminal scene-lease results after releasing the scene lock.
///
/// `SceneGraph::expire_leases()` performs the authoritative transition and
/// cleanup. This bridge only hands its immutable results to session handlers,
/// which own the transactional wire responses for connected agents.
fn publish_lease_expiries(
    lease_expirations: Option<&tze_hud_protocol::session_server::LeaseExpirySender>,
    expiries: Vec<tze_hud_scene::types::LeaseExpiry>,
) {
    let Some(lease_expirations) = lease_expirations else {
        return;
    };
    for expiry in expiries {
        let _ = lease_expirations.publish(expiry.into());
    }
}

// ─── WindowedRuntime ─────────────────────────────────────────────────────────

/// Shared state passed from the windowed runtime builder to the winit app.
///
/// All fields are `Arc`-wrapped or `Send` so the app handler can be moved into
/// the winit event loop.
#[allow(dead_code)] // several fields are read by the compositor/shutdown path; not all are used yet
struct WindowedRuntimeState {
    config: WindowedConfig,
    wake: WindowedWake,
    scheduled_main_deadline: Option<Deadline>,
    /// Compositor thread handle (stored so it can be joined on shutdown).
    compositor_handle: Option<std::thread::JoinHandle<()>>,
    /// Network runtime for gRPC / MCP.
    ///
    /// Kept alive for the duration of the windowed runtime. Dropping this
    /// shuts down all network tasks (gRPC server, future MCP bridge).
    network_rt: Option<NetworkRuntime>,
    /// Network task join handles (gRPC server tasks spawned onto `network_rt`).
    ///
    /// Stored so they can be aborted on shutdown. Dropping the `JoinHandle`
    /// does not kill the task; call `.abort()` explicitly.
    network_handles: Vec<tokio::task::JoinHandle<()>>,
    /// Immutable runtime context (capability policy, profile budgets).
    runtime_context: SharedRuntimeContext,
    /// Keeps the durable runtime widget asset store alive for runtime lifetime.
    _runtime_widget_store: Option<RuntimeWidgetStore>,
    /// Shared scene + session state.
    shared_state: Arc<Mutex<SharedState>>,
    /// Lock-free mirror of `SharedState.safe_mode_active` for the winit event thread.
    ///
    /// Cloned from `SharedState.safe_mode_atomic` at construction.  The event-thread
    /// dispatch path (`dispatch_key_down_event`, `dispatch_key_up_event`,
    /// `dispatch_character_event`) reads this flag with `Ordering::Acquire` to check
    /// safe-mode capture without ever acquiring the async Tokio `SharedState` mutex.
    ///
    /// Writers (`SafeModeController::enter_safe_mode` / `exit_safe_mode`) update
    /// both `SharedState.safe_mode_active` (under the mutex) and this AtomicBool
    /// (also under the mutex, with `Ordering::Release`).
    safe_mode_atomic: Arc<std::sync::atomic::AtomicBool>,
    /// Lock-free mirror of `scene.active_tab` for the winit event thread.
    ///
    /// Cloned from `SharedState.active_tab_mirror` at construction.  The
    /// keyboard-dispatch path reads this (via `active_tab_for_keyboard_dispatch`)
    /// instead of `try_lock`ing the scene Tokio mutex, so composer keystroke
    /// echo is never starved by gRPC scene-mutation batches (hud-dwcr7).
    active_tab_mirror: Arc<std::sync::Mutex<Option<tze_hud_scene::SceneId>>>,
    /// Shared chrome state — read by `ChromeRenderer`, written by `SafeModeController`.
    ///
    /// Created at runtime startup alongside `shared_state`.  Passed to
    /// `SafeModeController` so the hotkey bridge can enter/exit safe mode
    /// without going through the gRPC path.
    chrome_state: Arc<std::sync::RwLock<crate::shell::ChromeState>>,
    /// Input channel (ring buffer) — main thread writes, compositor thread reads.
    input_ring: Arc<std::sync::Mutex<std::collections::VecDeque<InputEvent>>>,
    /// Pending Stage 1/2 input latency samples for the next compositor frame.
    pending_input_latency: PendingInputLatencySamples,
    /// Frame-ready signal: compositor → main thread.
    frame_ready_rx: FrameReadyRx,
    /// Frame-ready sender (compositor thread will own this; stored here until
    /// the compositor thread is spawned and takes it).
    frame_ready_tx: Option<FrameReadyTx>,
    /// Present-ack broadcast sender (hud-4va6q). Cloned from the gRPC session
    /// service during network start; each compositor generation receives its own
    /// clone and emits `FramePresented` after each presented frame. `None` in
    /// compositor-only mode (grpc_port == 0), where there is no session to
    /// subscribe — the compositor still drains the present-ack queue.
    frame_presented_tx:
        Option<tokio::sync::broadcast::Sender<tze_hud_protocol::proto::FramePresented>>,
    /// Runtime-owned degradation-notice lane. Each compositor generation gets
    /// a clone so a runtime mode switch does not sever connected sessions from
    /// later degradation transitions.
    degradation_notices: Option<tze_hud_protocol::session_server::DegradationNoticeSender>,
    /// Durable runtime-to-session lane for terminal `SceneGraph::expire_leases`
    /// results. Each compositor generation receives a clone while the runtime
    /// retains ownership across window/surface recreation.
    lease_expirations: Option<tze_hud_protocol::session_server::LeaseExpirySender>,
    /// Compositor and surface (Some until compositor thread is spawned and takes the compositor).
    compositor: Option<Compositor>,
    /// The window surface (main thread owns this for the lifetime of the window).
    window_surface: Option<Arc<WindowSurface>>,
    /// Input processor for local feedback.
    input_processor: InputProcessor,
    /// Session-plane pointer capture commands delivered from the gRPC server.
    input_capture_rx:
        tokio::sync::mpsc::UnboundedReceiver<tze_hud_protocol::session::InputCaptureCommand>,
    pending_input_capture_commands:
        std::collections::VecDeque<tze_hud_protocol::session::InputCaptureCommand>,
    /// Focus manager — tracks which node / tile has keyboard focus per tab.
    ///
    /// Updated on every pointer-down via `InputProcessor::process_with_focus`.
    /// Consulted by the keyboard drain path to route `KeyboardProcessor` output
    /// to the correct agent session.
    focus_manager: FocusManager,
    /// Keyboard processor — translates raw OS key/char events into typed
    /// `KeyboardDispatch` descriptors when a node or tile has focus.
    ///
    /// Stateless with respect to focus; focus is owned by `focus_manager`.
    keyboard_processor: KeyboardProcessor,
    /// Telemetry collector.
    telemetry: TelemetryCollector,
    /// Frame pipeline (ArcSwap hit-test snapshot, overflow counters).
    pipeline: FramePipeline,
    /// Shutdown token.
    shutdown: ShutdownToken,
    /// Set when benchmark output failed after the event loop was already running.
    benchmark_failed: Arc<std::sync::atomic::AtomicBool>,
    /// Set by the compositor thread when a queued surface recovery cannot
    /// resume. `WindowedRuntime::run` maps this to a non-zero process result
    /// after the existing orderly event-loop cleanup completes.
    terminal_surface_recovery_failed: Arc<std::sync::atomic::AtomicBool>,
    /// Optional event-driven quiescent-efficiency measurement, initialized only
    /// after a real compositor and window surface exist.
    quiescent_efficiency: Option<WindowedQuiescentEfficiencyRunState>,
    /// Current cursor position (updated by CursorMoved events).
    cursor_x: f32,
    cursor_y: f32,
    /// True while the primary pointer button is down.
    ///
    /// Overlay hit-testing must remain captured during a press/drag sequence so
    /// Windows delivers the matching button release even if the cursor leaves the
    /// original hit region.
    left_button_down: bool,
    /// Last OS cursor-icon applied for portal resize/move affordances.
    ///
    /// Gates redundant `Window::set_cursor` calls so a steady stream of
    /// `PointerMove` events re-applies the platform cursor only when the
    /// affordance under the pointer actually changes (hud-g5yu1).
    cursor_tracker: CursorIconTracker,
    /// Winit window handle (Some after window is created).
    window: Option<Arc<Window>>,
    /// Effective window mode after platform fallback resolution.
    ///
    /// This may differ from `config.window.mode` if an overlay-to-fullscreen
    /// fallback occurred (e.g., GNOME Wayland with no layer-shell).
    effective_mode: WindowMode,
    /// Active hit-regions for overlay input passthrough.
    ///
    /// In overlay mode, the cursor hittest is toggled on/off per frame based
    /// on whether the cursor is inside any of these regions.  Empty means all
    /// events pass through.
    hit_regions: Vec<HitRegion>,
    /// Whether the overlay currently captures the pointer (last
    /// `set_cursor_hittest` value). While `false` the OS delivers no pointer
    /// events, so a low-rate cursor poll is the only way to notice the cursor
    /// entering an interactive region.
    overlay_capturing: bool,
    /// External static hit-regions configured by callers.
    static_hit_regions: Vec<HitRegion>,
    /// Runtime-managed widget hover trackers keyed by widget instance_name.
    widget_hover_trackers: std::collections::HashMap<String, WidgetHoverTracker>,
    /// Pending mode switch requested at runtime (disruptive — triggers surface
    /// recreation on the next event loop tick).
    pending_mode_switch: Option<WindowMode>,
    /// Pending widget SVG assets to register with the compositor after
    /// `init_widget_renderer` is called. Consumed once during first `resumed()`.
    pending_widget_svgs: Vec<crate::widget_startup::WidgetSvgAsset>,
    /// Tracked modifier key state for shortcut detection.
    modifiers: winit::keyboard::ModifiersState,
    /// Current monitor index for Ctrl+Shift+F8/F9 cycling.
    current_monitor_index: usize,
    /// Global design token map from scene startup.
    ///
    /// Stashed here after `run_scene_startup` returns so it can be applied
    /// to the compositor via `set_token_map` when the compositor is created in
    /// `resumed()`. After that call the field is no longer needed but is kept
    /// for potential hot-reload use.
    global_tokens: std::collections::HashMap<String, String>,
    /// Broadcast sender for `ElementRepositionedEvent`.
    ///
    /// Cloned from the `HudSessionImpl` after creation. `None` when gRPC is
    /// disabled (grpc_port == 0) or before network services start.
    ///
    /// Used by the windowed runtime to broadcast reset events without holding
    /// the async session lock (the reset path is sync chrome-layer code).
    element_repositioned_tx:
        Option<tokio::sync::broadcast::Sender<tze_hud_protocol::proto::ElementRepositionedEvent>>,
    /// Traffic-class-aware sender for runtime-injected input event batches.
    ///
    /// Cloned from the `HudSessionImpl` after creation. `None` when gRPC is
    /// disabled (grpc_port == 0) or before network services start.
    ///
    /// Used by the windowed runtime to dispatch any `EventBatch` to agents —
    /// scroll offset changes, keyboard down/up/character events, and future
    /// input event types. Transactional variants use the durable lane while
    /// ephemeral/state-stream variants remain bounded. Each pair is delivered
    /// only to the session handler whose namespace matches, filtered by
    /// `INPUT_EVENTS` subscription.
    input_event_tx: Option<tze_hud_protocol::session_server::InputEventSender>,
    /// Delivery context captured at the
    /// moment a composer node loses focus (blur transition).
    ///
    /// When `InputProcessor::process_with_focus` processes a focus-lost event
    /// for a composer region it calls `ComposerDraftManager::on_focus_lost()`,
    /// which clears `focused_node` and stores the terminal draft batch in
    /// `pending_flushed_batch`.  By the time `flush_composer_draft_at_settle`
    /// runs later that same frame, `composer_focused_node()` already returns
    /// `None` — so `composer_delivery_context()` cannot resolve the namespace
    /// or node_id.  Without this field the pending batch would be silently
    /// dropped, violating the §4.3 flush guarantee on blur.
    ///
    /// This field is written by the focus-transition handler immediately after
    /// `process_with_focus` returns (while the namespace and node_id are still
    /// available from the `FocusTransition`) and consumed by
    /// `flush_composer_draft_at_settle` as a fallback delivery context.
    ///
    /// Cleared on focus-gain to prevent stale context from leaking across
    /// focus boundaries.
    pending_blur_delivery_context: Option<ComposerDeliveryContext>,
    /// `(tile_id, anchor_byte)` for an in-progress composer pointer drag-select
    /// (hud-etrs0).
    ///
    /// Set on `PointerDown` when the hit lands on the focused composer node,
    /// to that node's owning tile plus the byte computed for the down
    /// position. `tile_id` is retained (rather than re-deriving it from each
    /// Move event's own hit-test, which may miss once the drag leaves the
    /// node's bounds) so `PointerMove` can keep converting pointer coordinates
    /// into the same tile-local space and extend the draft selection from
    /// `anchor_byte` to the byte under the current pointer position via
    /// `set_pointer_selection`. `PointerUp` clears it (the selection itself
    /// remains — only the drag gesture ends). Also cleared when composer
    /// focus is lost mid-drag so a stale anchor cannot leak into the next
    /// focused composer.
    composer_pointer_drag_anchor: Option<(tze_hud_scene::SceneId, usize)>,
    /// Per-portal resize state machines keyed by tile `SceneId`.
    ///
    /// Holds `PortalResizeState` for every portal tile that has been focused
    /// at least once. Created lazily on the first hotkey resize for a given
    /// portal tile; retained across keystrokes to maintain the monotonic
    /// sequence counter (which the adapter uses to detect skipped snapshots).
    ///
    /// Entries are pruned from the map when their tile is no longer present in
    /// the scene (see `prune_portal_resize_states`). Any in-flight gesture is
    /// abandoned cleanly because the tile is gone; the monotonic counter is
    /// discarded with the entry.
    portal_resize_states: std::collections::HashMap<tze_hud_scene::SceneId, PortalResizeState>,
    /// Resize chord identities whose `KeyDown` was consumed by the focused-portal
    /// hotkey path and whose matching `KeyUp` must therefore be swallowed.
    ///
    /// Live Windows (SendInput) can deliver release-only resize key streams, so
    /// `dispatch_key_up_event_inner` resizes on key-up as a fallback (hud-v4k1h).
    /// This set keeps a normal physical down/up pair to exactly one resize: the
    /// key-down inserts the chord here, the matching key-up removes it instead of
    /// resizing again.
    consumed_portal_resize_keydowns: std::collections::HashSet<String>,
    /// Shell-reserved chord identities whose `KeyDown` was handled locally.
    /// Their matching `KeyUp` is consumed before focused-agent routing so an
    /// agent never observes half of a chrome-owned key sequence.
    consumed_shell_shortcut_keydowns: std::collections::HashSet<String>,
    /// Focused nodes holding local pressed feedback from a keyboard ACTIVATE,
    /// keyed by physical key identity until the matching key release.
    keyboard_activation_nodes: std::collections::HashMap<String, tze_hud_scene::SceneId>,
    /// Non-navigation command bindings whose raw key-down was replaced by a
    /// `CommandInputEvent`; their matching raw key-up must be swallowed too.
    consumed_command_keydowns: std::collections::HashSet<String>,
    /// Shared local composer echo state for the compositor thread (hud-r3ax6).
    ///
    /// Written by the input-event thread (this thread) on every keystroke that
    /// mutates the composer draft.  The compositor thread drains it once per
    /// frame at frame start via `drain_local_composer_state`.
    ///
    /// `Some(Some(state))` = new draft snapshot; `Some(None)` = deactivate.
    /// `None` = no update since last drain (compositor keeps prior state).
    local_composer_state: LocalComposerStateHandle,
    /// Shared queue for runtime-authored viewer reply echoes on raw-tile portals
    /// (hud-nx7yq.3).  On an accepted composer submission for a raw tile (one not
    /// attached to the projection authority, which echoes on its own path), this
    /// thread pushes the submitted text; the compositor drains it into its
    /// per-tile viewer-echo store and renders it above the composer strip.
    viewer_echo_queue: PortalViewerEchoQueue,
    /// Last conservative geometry used for each raw-tile history seed. Retained
    /// history is tracked in physical pixels by InputProcessor, so a later
    /// whole-portal resize rebases its conservative tail estimate for both line
    /// pitch and wrap capacity before adding the next local echo.
    input_history_seed_states:
        std::collections::HashMap<tze_hud_scene::SceneId, portal::InputHistorySeedState>,
    /// Shared handle carrying the current keyboard-focus owner to the compositor's
    /// chrome-layer ring pass (hud-k6yvb). Written each frame in `about_to_wait`
    /// from the active tab's `FocusManager` owner; the compositor draws the ring
    /// for whatever owner it names (node or tile-level), above all content.
    focus_ring_owner_state: FocusRingOwnerHandle,
    /// Shared handle carrying the portal tile whose resize-grip corner the pointer
    /// is over to the compositor's grip pass (hud-wgiys). Written each frame in
    /// `about_to_wait`: the focused portal's `SceneId` when the pointer sits over
    /// its bottom-right resize corner, else `None`. The compositor swaps that
    /// tile's grip mark to `hover_color`. Cloned from
    /// `compositor.resize_grip_hover_state` at init.
    resize_grip_hover_state: ResizeGripHoverHandle,
    /// Shared handle carrying the tile the pointer is over, whose viewer close
    /// button the compositor shows (hud-jm8nq.11). Written each `about_to_wait`
    /// from the polled cursor; cloned from `compositor.tile_close_hover_state`.
    tile_close_hover_state: TileCloseHoverHandle,
    /// True after `CursorLeft` until the next `CursorMoved`: the pointer is
    /// outside the window, so no tile is hovered.
    cursor_left_window: bool,
    /// Reverse channel (hud-21o6x): the compositor publishes the active composer's
    /// wrapped-line layout here each frame; this (main) thread reads it before
    /// dispatching ArrowUp/ArrowDown so the caret can step between soft-wrapped
    /// visual rows. Cloned from `compositor.composer_visual_layout` at init.
    composer_visual_layout: tze_hud_compositor::ComposerVisualLayoutHandle,
    /// In-process portal projection authority driver (hud-2iup7).
    ///
    /// Hosts a `ProjectionAuthority` in the runtime process and drives the portal
    /// drain loop on each `about_to_wait` call.  Wires
    /// `InputProcessor::notify_tile_content_appended` so follow-tail advances
    /// (spec §3.2) and scrolled-back stability is preserved (spec §3.3).
    portal_projection_driver: crate::portal_projection_driver::InProcessPortalDriver,
    /// Receiver for [`PortalOp`] messages sent from the MCP HTTP task (hud-bq0gl.2).
    ///
    /// The MCP async task sends the projection-lifecycle `PortalOp` values
    /// (`Attach`, `PublishOutput`, `GetPendingInput`, `AcknowledgeInput`,
    /// `Detach`) through this channel.  The winit event-loop thread drains it via
    /// `drain_portal_ops()` on each `about_to_wait` iteration, before the normal
    /// `drain_portal_projection()` call, so content published in the same
    /// event-loop tick is also coalesced by the cadence coalescer and materialised
    /// into the scene within the same frame.  The sender end is threaded to
    /// `McpServer` via `start_mcp_http_server`.
    ///
    /// `None` when the MCP server is disabled (`mcp_port == 0`) or when the
    /// network runtime could not be created.
    portal_op_rx: Option<tokio::sync::mpsc::UnboundedReceiver<tze_hud_mcp::portal_op::PortalOp>>,
    /// Keyboard events deferred because the shared-state or scene lock was busy
    /// at dispatch time (hud-2fz34).
    ///
    /// When `dispatch_key_down_event`, `dispatch_key_up_event`, or
    /// `dispatch_character_event` cannot acquire the async Tokio mutex via
    /// `try_lock`, the raw event is pushed here rather than blocking the
    /// event-loop thread.  `drain_pending_keyboard_events` retries from
    /// `about_to_wait` once per iteration, matching the sibling deferral
    /// patterns in `drain_portal_projection` and `drain_input_capture_commands`.
    ///
    /// In normal operation the queue is empty; it only accumulates under the
    /// brief lock contention window caused by concurrent gRPC scene mutations.
    /// The queue is unbounded in the same sense as
    /// `pending_input_capture_commands` — bounded in practice by the number of
    /// keystrokes that arrive during a single lock-contention window.
    pending_keyboard_events: VecDeque<PendingKeyboardEvent>,
    /// Cumulative count of interactive-feedback scene updates dropped because the
    /// main-thread `spin_acquire` timed out on the scene / shared-state lock
    /// during a guaranteed-feedback gesture (drag-move / live resize) — the exact
    /// symptom the hud-uyhpn lock-scope fix targets (see
    /// [`INTERACTION_LOCK_BUDGET`]).
    ///
    /// This is the confirmation lever for the fix: with the compositor no longer
    /// holding the scene lock across the vsync-blocking present, this counter
    /// should stay at 0 during a live drag. It is surfaced through the existing
    /// best-effort `diag` log (throttled) and readable directly in tests.
    ///
    /// Incremented via interior mutability at the `spin_acquire` miss sites; the
    /// borrow of this field ends before any `&mut self` work in the same tick.
    interaction_feedback_lock_misses: std::sync::atomic::AtomicU64,
}

impl WindowedRuntimeState {
    /// Clone runtime-owned session notifiers for one compositor generation.
    ///
    /// These senders outlive a compositor: a runtime mode switch joins the old
    /// thread and starts a replacement against the same network session. Taking
    /// them here would make the replacement silently lose frame, degradation,
    /// and terminal-lease delivery.
    fn compositor_runtime_senders(
        &self,
    ) -> (
        Option<tokio::sync::broadcast::Sender<tze_hud_protocol::proto::FramePresented>>,
        Option<tze_hud_protocol::session_server::DegradationNoticeSender>,
        Option<tze_hud_protocol::session_server::LeaseExpirySender>,
    ) {
        (
            self.frame_presented_tx.clone(),
            self.degradation_notices.clone(),
            self.lease_expirations.clone(),
        )
    }
}

// ─── WinitApp ────────────────────────────────────────────────────────────────

/// `ApplicationHandler` implementation for winit 0.30 event loop.
///
/// The main thread creates the window in `resumed()`, initialises the
/// compositor + surface, spawns the compositor thread, then processes
/// window events on every `window_event()` call.
struct WinitApp {
    state: WindowedRuntimeState,
}

/// Block a dedicated availability waiter until both locks needed by main-loop
/// scene work have become obtainable, then return immediately. This function is
/// only called off the winit event thread by [`WindowedWake`]'s coalesced
/// completion waiter; the event loop itself always uses `try_lock`.
fn wait_for_shared_scene_availability(shared_state: Arc<Mutex<SharedState>>) {
    let shared = shared_state.blocking_lock();
    let scene = Arc::clone(&shared.scene);
    drop(shared);
    drop(scene.blocking_lock());
}

/// Translate a portal authority deadline into an event-loop wait.
///
/// A past deadline normally gets one immediate turn after a successful drain.
/// When that drain could not acquire the scene, however, retrying the same
/// past deadline would be correctness polling; the caller installs a
/// completion-driven availability wake instead.
fn portal_deadline_after_drain(
    portal_deadline: PortalWakeDeadline,
    now: Instant,
    now_wall_us: u64,
    portal_drain: PortalProjectionDrain,
) -> Option<Deadline> {
    let remaining_us = portal_deadline.wall_us.saturating_sub(now_wall_us);
    if remaining_us == 0 {
        (!portal_drain.is_deferred())
            .then_some(Deadline::new(now, portal_deadline.family.wakeup_source()))
    } else {
        // Keep the wall-clock observation paired with `now`. Calling
        // `deadline_from_wall_us` here would take a second wall-clock sample;
        // a deadline crossing between the two reads could become an accidental
        // immediate retry while the portal drain is still deferred.
        Some(Deadline::new(
            now + std::time::Duration::from_micros(remaining_us),
            portal_deadline.family.wakeup_source(),
        ))
    }
}

/// Preserve the runtime's terminal GPU-loss shutdown path when a compositor
/// surface recovery cannot resume normal presentation.
///
/// The main event loop observes the shutdown token on its next proxy wake and
/// exits cleanly; `ShutdownReason::GpuDeviceLost` retains the existing non-zero
/// graceful-shutdown classification without changing any agent-facing API.
fn shutdown_after_terminal_surface_recovery(
    shutdown: &crate::threads::ShutdownToken,
    wake: &WindowedWake,
    terminal_surface_recovery_failed: &std::sync::atomic::AtomicBool,
) {
    tracing::error!("window surface recovery failed terminally; requesting GPU-loss shutdown");
    terminal_surface_recovery_failed.store(true, std::sync::atomic::Ordering::Release);
    shutdown.trigger(crate::threads::ShutdownReason::GpuDeviceLost);
    wake.notify_main(crate::idle_efficiency::RuntimeWakeupSource::Shutdown);
}

impl WinitApp {
    /// Arrange one completion-driven retry for main-thread work that could not
    /// inspect or mutate the shared scene because either async mutex was held.
    ///
    /// This deliberately runs the blocking waits on the dedicated availability
    /// worker, never on winit's event thread.  Coalescing lives in
    /// [`WindowedWake`], so multiple busy observations while the same holder is
    /// active still create only one release-driven retry.
    fn schedule_shared_scene_availability_wake(&self) {
        let shared_state = Arc::clone(&self.state.shared_state);
        self.state.wake.schedule_main_after_availability(move || {
            wait_for_shared_scene_availability(shared_state);
        });
    }

    /// Claim a scheduled deadline that became due before or during this event
    /// loop turn. `WaitCancelled` may arrive just before the deadline and the
    /// settle work may cross it, so `ResumeTimeReached` cannot be the only claim
    /// point.
    fn claim_due_scheduled_main_deadline(
        &mut self,
        now: Instant,
    ) -> Option<wake::MainWorkCheckpoint> {
        let deadline = self
            .state
            .scheduled_main_deadline
            .filter(|deadline| deadline.at <= now)?;
        self.state.scheduled_main_deadline = None;
        // This deadline piggybacks on the already-counted proxy/OS event that
        // cancelled the wait. Only `ResumeTimeReached` records a timer-caused
        // main-loop wakeup.
        Some(self.state.wake.mark_main_work_pending(deadline.source))
    }
}

impl WinitApp {
    /// Settle the scene-side main-thread work for one event-loop turn: flush
    /// the composer draft, retry deferred keystrokes, dispatch MCP portal ops,
    /// and drain the portal projection into the scene. Needs no window or
    /// GPU, so the headless harness runs exactly this sequence too.
    fn settle_scene_work(&mut self) -> PortalProjectionDrain {
        // Flush any coalesced composer draft notifications accumulated during
        // the current event batch.  This is the normal settle point: all key
        // events for this winit iteration have been drained above; flushing here
        // guarantees the terminal draft state is delivered within the same batch
        // window (spec §4.3 flush guarantee).
        self.flush_composer_draft_at_settle();
        // Opportunistically reconverge the lock-free active_tab mirror from the
        // authoritative scene (hud-dwcr7).  The mirror is also refreshed at the
        // point of every active_tab change (gRPC mutation apply, pointer-down
        // tab switch), but this best-effort per-frame sync is a safety net so a
        // mirror can never drift indefinitely from any tab-change path.  Uses
        // try_lock — never stalls the event loop; simply skips this frame if the
        // scene lock is momentarily busy.
        self.refresh_active_tab_mirror_opportunistic();
        // Publish the active tab's current focus owner to the compositor's
        // chrome-layer ring pass (hud-k6yvb). Per-frame + latest-wins so the ring
        // tracks Tab/click/Escape focus changes without instrumenting every
        // transition site; the compositor recomputes bounds from the live scene,
        // so geometry changes (resize/drag) stay fresh without a focus event.
        self.clear_focus_on_removed_tile();
        self.push_focus_ring_owner();
        // Publish the resize-grip hover target (hud-wgiys): when the pointer sits
        // over the focused portal's bottom-right resize corner, the compositor
        // lights that tile's grip in hover_color. Same per-frame + latest-wins
        // cadence as the focus-ring push above.
        self.push_resize_grip_hover();
        // Publish the tile under the pointer so the compositor shows its viewer
        // close button (hud-jm8nq.11). Same per-frame + latest-wins cadence;
        // an unchanged target costs the compositor nothing.
        self.push_tile_close_hover();
        // Retry any keyboard events that were deferred because the scene lock
        // was busy during dispatch (hud-2fz34).  Runs after composer flush so
        // deferred keystrokes re-enter the same path as fresh ones.
        self.drain_pending_keyboard_events();
        // Drain any PortalOp messages from the MCP channel (hud-bq0gl.2).
        // Must run BEFORE drain_portal_projection so that Attach/PublishOutput
        // ops enqueued in the same event-loop tick are fed into the cadence
        // coalescer and materialised by the immediately-following drain call.
        self.drain_portal_ops();
        // Drain the in-process portal projection authority (hud-2iup7).
        // Must run AFTER composer flush so draft state is settled before portal
        // content is refreshed.  Uses try_lock on the scene to avoid blocking
        // the main thread (deferred to next about_to_wait if busy).
        let portal_drain = self.drain_portal_projection();
        // Prune stale portal_resize_states entries for tiles removed from the
        // scene (hud-kgu8u). Uses try_lock; silently deferred if lock is busy.
        self.prune_portal_resize_states();
        portal_drain
    }
}

impl ApplicationHandler<RuntimeWakeEvent> for WinitApp {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        match cause {
            StartCause::ResumeTimeReached { .. } => {
                if self
                    .state
                    .scheduled_main_deadline
                    .is_some_and(|deadline| deadline.at <= Instant::now())
                {
                    let deadline = self
                        .state
                        .scheduled_main_deadline
                        .take()
                        .expect("a due main deadline was checked above");
                    self.state
                        .wake
                        .counters()
                        .record_main_wakeup(deadline.source);
                    // Main-owned deadlines (portal liveness/cadence, hover, and
                    // chrome expiry) mutate the scene later in `about_to_wait`.
                    // Mark a post-mutation compositor notification as owed; a
                    // pre-mutation wake can otherwise be consumed against the
                    // old scene and strand the newly-created work.
                    self.state.wake.mark_main_work_pending(deadline.source);
                }
            }
            StartCause::WaitCancelled { .. } => {
                // EventLoopProxy delivery also cancels a parked wait, but that
                // wake is runtime-requested and is counted by `user_event`.
                // Only an unattributed cancellation belongs in the separate
                // operating-system bucket.
                if !self.state.wake.has_pending_proxy_event() {
                    self.state
                        .wake
                        .counters()
                        .record_excluded_operating_system_wakeup();
                }
            }
            StartCause::Init | StartCause::Poll => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: RuntimeWakeEvent) {
        let source = self.state.wake.take_main_source();
        self.state.wake.counters().record_main_wakeup(source);
    }

    /// Called by winit when the event loop has processed all pending events for
    /// the current iteration.  We use this to apply any pending mode switch:
    /// tearing down the current window/compositor and re-initialising with the
    /// new mode is safe here because no window events are in flight.
    ///
    /// Note: `resumed()` is a *lifecycle* callback (initial app start / app
    /// resume after suspension) and is NOT triggered by `window.request_redraw()`.
    /// Pending mode switches must therefore be handled here in `about_to_wait`
    /// rather than in `resumed()`.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // A proxy/OS event can cancel WaitUntil immediately before its instant,
        // then settle work can cross the deadline. Claim it before the work
        // checkpoint so its typed post-settle compositor wake cannot be lost.
        let _ = self.claim_due_scheduled_main_deadline(Instant::now());
        // Acknowledge only producer work that was visible before this drain.
        // Notifications racing after this checkpoint retain a later generation
        // for the next event-loop turn and cannot be erased at settle time.
        let main_work_checkpoint = self.state.wake.main_work_checkpoint();
        if self.state.shutdown.is_triggered() {
            event_loop.exit();
            return;
        }
        if self.state.pending_mode_switch.is_some() {
            self.apply_pending_mode_switch();
            // Re-create the window with the new mode by forwarding to the
            // initialisation path inside resumed().
            self.resumed(event_loop);
        }
        self.refresh_cursor_position_from_os();
        self.drain_input_capture_commands();
        self.synthesize_left_release_if_physically_up();
        self.refresh_widget_hover_tracking();
        self.update_overlay_cursor_hittest();
        let portal_drain = self.settle_scene_work();
        let settled_scene_changed = portal_drain.scene_changed();

        // ── Per-frame ticks + present poll (hud-ilivg) ────────────────────
        // Moved here from the `RedrawRequested` handler so the main loop no
        // longer self-perpetuates a `request_redraw` every frame purely to drive
        // these. `about_to_wait` fires after each explicit event or scheduled
        // deadline (and hosts portal/input draining above). `maybe_present_frame`
        // is a cheap watch-channel poll that presents only when the compositor
        // signalled a new frame — with the compositor render gate that now
        // happens only when the scene changed or an animation is in flight.
        self.inject_windowed_benchmark_input_probe();
        self.tick_widget_hover_tracking();
        // Auto-dismiss the drag-handle context menu after 3 seconds.
        self.tick_context_menu_auto_dismiss();
        self.maybe_present_frame();

        let now = Instant::now();
        let counters = Arc::clone(self.state.wake.counters());
        let quiescent_completion = self
            .state
            .quiescent_efficiency
            .as_mut()
            .and_then(|measurement| measurement.advance(now, counters.as_ref()));
        if let Some(completion) = quiescent_completion {
            let measurement = self
                .state
                .quiescent_efficiency
                .as_ref()
                .expect("measurement must remain available until its artifact is written");
            let artifact_path = measurement.emit_path().to_path_buf();
            let write_result = measurement.write(&completion);
            if let Err(error) = write_result {
                tracing::error!(
                    %error,
                    path = %artifact_path.display(),
                    "failed to write quiescent-efficiency runtime artifact"
                );
                self.state
                    .benchmark_failed
                    .store(true, std::sync::atomic::Ordering::Release);
            } else if completion.validation.passed {
                tracing::info!(
                    path = %artifact_path.display(),
                    wakeups = completion.artifact.wakeups.combined_runtime_driven,
                    "quiescent-efficiency runtime artifact written"
                );
            } else {
                tracing::error!(
                    path = %artifact_path.display(),
                    violations = ?completion.validation.violations,
                    "quiescent-efficiency runtime artifact failed validation"
                );
                self.state
                    .benchmark_failed
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            self.state
                .shutdown
                .trigger(crate::threads::ShutdownReason::Clean);
            event_loop.exit();
            return;
        }

        let deadline_crossed_during_settle = self.claim_due_scheduled_main_deadline(now);
        let mut deadlines = Vec::new();
        if let Some(at) =
            crate::widget_hover::next_hover_deadline(&self.state.widget_hover_trackers)
        {
            deadlines.push(Deadline::new(
                at,
                crate::idle_efficiency::RuntimeWakeupSource::AnimationDeadline,
            ));
        }
        let mut inspected_scene_deadlines = false;
        if let Ok(state) = self.state.shared_state.try_lock()
            && let Ok(scene) = state.scene.try_lock()
        {
            inspected_scene_deadlines = true;
            if let Some(portal_deadline) = self
                .state
                .portal_projection_driver
                .next_wake_deadline(&scene)
            {
                let now_wall_us = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_micros() as u64)
                    .unwrap_or(0);
                if let Some(deadline) =
                    portal_deadline_after_drain(portal_deadline, now, now_wall_us, portal_drain)
                {
                    deadlines.push(deadline);
                }
            }
            if let Some(menu) = &scene.overlay.drag_handle_context_menu {
                const AUTO_DISMISS_NS: u64 = 3_000_000_000;
                let remaining_ns = menu
                    .shown_at_ns
                    .saturating_add(AUTO_DISMISS_NS)
                    .saturating_sub(nanoseconds_since_start());
                deadlines.push(Deadline::new(
                    now + std::time::Duration::from_nanos(remaining_ns),
                    crate::idle_efficiency::RuntimeWakeupSource::TtlDeadline,
                ));
            }
        }
        let snapshot = self.state.pipeline.hit_test_snapshot.load();
        // A dismissible tile counts as interactive: in passthrough the pointer
        // must be polled to show its viewer close button (hud-jm8nq.11).
        let interactive = !self.state.hit_regions.is_empty()
            || !snapshot.drag_handles.is_empty()
            || snapshot
                .tiles
                .iter()
                .any(|tile| tile.has_scroll_config || tile.dismissible);
        deadlines.extend(wake::cursor_poll_deadline(
            now,
            self.state.effective_mode == WindowMode::Overlay,
            interactive,
            !self.state.overlay_capturing,
        ));
        let has_deferred_scene_work = portal_drain.is_deferred()
            || !self.state.pending_input_capture_commands.is_empty()
            || !self.state.pending_keyboard_events.is_empty();
        if !inspected_scene_deadlines || has_deferred_scene_work {
            // Lock contention is not a deadline. Install one coalesced waiter
            // that wakes the main loop only after the shared scene is actually
            // available, rather than correctness-polling every millisecond.
            self.schedule_shared_scene_availability_wake();
        }
        let next = deadlines.iter().copied().min_by_key(|deadline| deadline.at);
        self.state.scheduled_main_deadline = next;
        let quiescent_deadline = self
            .state
            .quiescent_efficiency
            .as_ref()
            .and_then(WindowedQuiescentEfficiencyRunState::next_deadline);
        let control_flow = match (
            control_flow_for_deadlines(now, deadlines),
            quiescent_deadline,
        ) {
            (ControlFlow::Wait, Some(deadline)) => ControlFlow::WaitUntil(deadline),
            (ControlFlow::WaitUntil(main_deadline), Some(deadline)) => {
                ControlFlow::WaitUntil(main_deadline.min(deadline))
            }
            (ControlFlow::Poll, Some(_)) => ControlFlow::Poll,
            (control_flow, None) => control_flow,
        };
        event_loop.set_control_flow(control_flow);
        if let Some(deadline_checkpoint) = deadline_crossed_during_settle {
            self.state.wake.finish_main_work(main_work_checkpoint);
            self.state
                .wake
                .finish_main_work_after_settle(deadline_checkpoint, settled_scene_changed);
        } else {
            self.state
                .wake
                .finish_main_work_after_settle(main_work_checkpoint, settled_scene_changed);
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.window.is_some() {
            return; // Already initialised.
        }

        // ── Create winit window ────────────────────────────────────────────
        // Clone the title and snapshot configured dimensions before any mutation
        // to avoid borrow conflicts when we later update the config in-place for
        // overlay auto-sizing.
        let window_title = self.state.config.window.title.clone();
        let cfg_width = self.state.config.window.width;
        let cfg_height = self.state.config.window.height;

        // Build window attributes based on the effective window mode.
        //
        // Fullscreen: borderless fullscreen — compositor owns the entire display
        //   with an opaque background. All input captured. Spec §Fullscreen mode
        //   (line 177).
        //
        // Overlay: transparent, borderless, always-on-top window with per-region
        //   input passthrough via set_cursor_hittest(). Spec §Overlay click-through
        //   (line 181).
        let attrs = match self.state.effective_mode {
            WindowMode::Fullscreen => {
                tracing::info!(
                    "window mode: fullscreen (borderless) — compositor owns display, all input captured"
                );
                WindowAttributes::default()
                    .with_title(window_title)
                    // Borderless fullscreen on the current monitor.
                    .with_fullscreen(Some(Fullscreen::Borderless(None)))
                    .with_decorations(false)
            }
            WindowMode::Overlay => {
                // Determine overlay window dimensions.
                //
                // When `overlay_auto_size` is true (the default), query the primary
                // monitor's physical size via the event loop and use it as the window
                // dimensions.  This ensures the overlay covers the full display on any
                // monitor (1080p, 1440p, 4K, etc.) without requiring explicit
                // --width/--height flags.
                //
                // Fall back to the configured width/height if monitor detection fails
                // (headless environments, missing display server, etc.).
                let (overlay_w, overlay_h, mon_x, mon_y) = if self.state.config.overlay_auto_size {
                    detect_monitor_size(
                        event_loop,
                        cfg_width,
                        cfg_height,
                        self.state.config.monitor_index,
                    )
                } else {
                    (cfg_width, cfg_height, 0, 0)
                };

                // Update the config so that downstream code (surface init, logging)
                // sees the resolved dimensions rather than the stale defaults.
                self.state.config.window.width = overlay_w;
                self.state.config.window.height = overlay_h;

                tracing::info!(
                    width = overlay_w,
                    height = overlay_h,
                    position_x = mon_x,
                    position_y = mon_y,
                    auto_size = self.state.config.overlay_auto_size,
                    "window mode: overlay/HUD — transparent borderless always-on-top"
                );
                #[cfg(target_os = "windows")]
                {
                    use winit::platform::windows::WindowAttributesExtWindows;
                    WindowAttributes::default()
                        .with_title(window_title)
                        .with_inner_size(winit::dpi::PhysicalSize::new(overlay_w, overlay_h))
                        .with_position(winit::dpi::PhysicalPosition::new(mon_x, mon_y))
                        .with_transparent(true)
                        .with_decorations(false)
                        .with_window_level(WindowLevel::AlwaysOnTop)
                        // Hide from taskbar so the overlay can't be
                        // accidentally minimized or alt-tabbed to.
                        .with_skip_taskbar(true)
                        // Set WS_EX_NOREDIRECTIONBITMAP at creation time —
                        // DWM will present the swapchain directly with
                        // per-pixel alpha from PreMultiplied mode.
                        .with_no_redirection_bitmap(true)
                }
                #[cfg(not(target_os = "windows"))]
                {
                    WindowAttributes::default()
                        .with_title(window_title)
                        .with_inner_size(winit::dpi::PhysicalSize::new(overlay_w, overlay_h))
                        .with_position(winit::dpi::PhysicalPosition::new(0i32, 0i32))
                        .with_transparent(true)
                        .with_decorations(false)
                        .with_window_level(WindowLevel::AlwaysOnTop)
                }
            }
        };

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!("failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };

        // In overlay mode, initialise cursor hittest to false so all pointer
        // events pass through to the desktop until the cursor enters a
        // hit-region.  The hittest is toggled per-frame in enqueue_pointer_event()
        // per spec §Overlay click-through (line 181).
        if self.state.effective_mode == WindowMode::Overlay {
            if let Err(e) = window.set_cursor_hittest(false) {
                tracing::warn!(
                    error = %e,
                    "overlay mode: set_cursor_hittest(false) failed — passthrough \
                     may not work on this platform/compositor"
                );
            }
            // WS_EX_NOREDIRECTIONBITMAP is set at creation time via
            // with_no_redirection_bitmap(true) above. No post-creation
            // flag manipulation needed.
        }

        self.state.window = Some(window.clone());
        self.refresh_widget_hover_tracking();

        let cfg = self.state.config.clone();
        let monitor_refresh_millihz = window
            .current_monitor()
            .and_then(|monitor| monitor.refresh_rate_millihertz());
        let degradation_effective_fps =
            crate::degradation::effective_degradation_fps(cfg.target_fps, monitor_refresh_millihz);
        tracing::info!(
            target_fps = cfg.target_fps,
            ?monitor_refresh_millihz,
            degradation_effective_fps,
            "windowed: froze startup degradation cadence"
        );
        let window_clone = window.clone();

        // ── Resolve actual surface dimensions ─────────────────────────────
        // Query the actual physical size of the window AFTER creation.
        // On Windows the OS may constrain the window to the monitor bounds or
        // apply DPI scaling, so `window.inner_size()` may differ from the
        // requested cfg.window.width/height.  Using the configured values
        // directly causes wgpu to configure the swapchain at a size that
        // doesn't match the surface handle's drawable area, which triggers a
        // validation panic before `surface.configure()` can write alpha_diag.txt.
        //
        // `window.inner_size()` returns `PhysicalSize<u32>` — physical pixels —
        // when per-monitor DPI awareness is active (guaranteed by the embedded
        // manifest in `tze_hud_app/tze_hud.manifest`). Do NOT multiply by
        // `scale_factor()`; that would over-count on DPI-scaled displays.
        //
        // Fall back to the configured values only when inner_size() returns (0,0)
        // (e.g., window not yet shown or minimized at construction time — rare
        // but possible on some Win32 driver/compositor combinations).
        let actual_size = window.inner_size();
        let scale = window.scale_factor(); // logged for diagnostics only
        let (surface_width, surface_height) = if actual_size.width > 0 && actual_size.height > 0 {
            (actual_size.width, actual_size.height)
        } else {
            tracing::warn!(
                requested_width = cfg.window.width,
                requested_height = cfg.window.height,
                "window.inner_size() returned (0,0) at creation; \
                 using configured dimensions as fallback"
            );
            (cfg.window.width, cfg.window.height)
        };
        tracing::info!(
            configured_width = cfg.window.width,
            configured_height = cfg.window.height,
            inner_width = actual_size.width,
            inner_height = actual_size.height,
            scale_factor = scale,
            surface_width,
            surface_height,
            "windowed: resolved surface dimensions from window.inner_size() (physical pixels)"
        );
        if surface_width > 0 && surface_height > 0 {
            self.state.config.window.width = surface_width;
            self.state.config.window.height = surface_height;
            if let Ok(state) = self.state.shared_state.try_lock() {
                if let Ok(mut scene) = state.scene.try_lock() {
                    sync_scene_display_area(&mut scene, surface_width, surface_height);
                    if self.state.config.benchmark.is_some() {
                        seed_windowed_benchmark_scene(&mut scene, surface_width, surface_height);
                    }
                }
            }
        }
        // Diagnostic: write surface resolution so remote operators can verify.
        let _ = std::fs::write(
            "C:\\tze_hud\\logs\\surface_diag.txt",
            format!(
                "configured={}x{} inner={}x{} scale={} surface={}x{}\n",
                cfg.window.width,
                cfg.window.height,
                actual_size.width,
                actual_size.height,
                scale,
                surface_width,
                surface_height,
            ),
        );

        // ── Create compositor + surface (async in a blocking context) ──────
        // We need an async context to call Compositor::new_windowed.
        // Use a temporary single-thread Tokio runtime here — this runs only
        // at startup and is dropped immediately after.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build startup tokio runtime");

        let is_overlay = self.state.effective_mode == WindowMode::Overlay;
        let constrained_measurement = cfg.quiescent_efficiency.is_some();
        let (mut compositor, window_surface) = rt.block_on(async {
            let mut c = if constrained_measurement {
                Compositor::new_windowed_constrained(
                    window_clone,
                    surface_width,
                    surface_height,
                    is_overlay,
                )
                .await
            } else if is_overlay {
                Compositor::new_windowed_overlay(window_clone, surface_width, surface_height).await
            } else {
                Compositor::new_windowed(window_clone, surface_width, surface_height).await
            }
            .expect("Compositor::new_windowed failed");
            c.0.overlay_mode = is_overlay;
            c.0.debug_zone_tints = self.state.config.debug_zones;
            c
        });

        // ── Initialize text renderer ──────────────────────────────────────
        // Must be called after surface configuration so we know the negotiated
        // swapchain format. glyphon text rendering is inert until this runs.
        let surface_format = window_surface
            .config
            .lock()
            .expect("WindowSurface config lock poisoned at text renderer init")
            .format;
        compositor.init_text_renderer(surface_format);
        compositor.init_widget_renderer(surface_format);
        compositor.set_resident_ledger(self.state.runtime_context.resident_ledger.clone());
        // Apply the resolved per-surface truncation-input bound so the
        // viewport-adjacent-window fallback engages at the operator-configured
        // threshold rather than the compositor's built-in default (hud-59p2z).
        compositor.set_max_truncation_input_bytes(
            self.state
                .runtime_context
                .profile
                .max_truncation_input_bytes as usize,
        );

        // Register pending widget SVG assets with the widget renderer.
        process_pending_widget_svgs(
            compositor.widget_renderer_mut(),
            self.state.pending_widget_svgs.drain(..),
        );
        tracing::info!(format = ?surface_format, "windowed: text + widget renderers initialized");

        // Apply resolved design tokens to the compositor so severity colors are
        // looked up from the token map at render time rather than using hardcoded
        // constants.  Clone the map so the state retains its copy for potential
        // future hot-reload use.
        compositor.set_token_map(self.state.global_tokens.clone());
        tracing::debug!(
            token_count = self.state.global_tokens.len(),
            "windowed: compositor token map applied"
        );

        // Propagate the startup design-token map to the in-process portal
        // projection driver (hud-be6ee).  This satisfies the acceptance criterion
        // that token wiring reaches live adapters: any projection session that
        // attaches after startup inherits the resolved visual tokens from the
        // runtime config rather than the driver's empty default map.
        //
        // The driver stores the map in `InProcessPortalDriveState::token_overrides`
        // and re-resolves `PortalVisualTokens` for every new adapter at attach
        // time, so this call is the only site needed for a single-startup-token
        // profile.  A hot-reload path would call `apply_token_map` again here
        // after updating `self.state.global_tokens`.
        self.state
            .portal_projection_driver
            .apply_token_map(self.state.global_tokens.clone());
        tracing::debug!(
            token_count = self.state.global_tokens.len(),
            "windowed: portal projection driver token map applied"
        );

        let window_surface = Arc::new(window_surface);
        self.state.window_surface = Some(window_surface.clone());

        // ── Elevate main thread priority ──────────────────────────────────
        crate::threads::elevate_main_thread_priority();

        // ── Wire local composer echo channel to the compositor (hud-r3ax6) ──
        // Clone the Arc from the compositor so the input-event thread (this
        // thread) can push draft snapshots to the compositor thread without any
        // additional allocations or locks on the hot path.
        self.state.local_composer_state = Arc::clone(&compositor.local_composer_state);
        self.state.viewer_echo_queue = Arc::clone(&compositor.viewer_echo_queue);
        self.state.focus_ring_owner_state = Arc::clone(&compositor.focus_ring_owner_state);
        self.state.resize_grip_hover_state = Arc::clone(&compositor.resize_grip_hover_state);
        self.state.tile_close_hover_state = Arc::clone(&compositor.tile_close_hover_state);
        // Reverse channel: read the compositor's per-frame wrapped-line layout for
        // soft-wrap vertical caret movement (hud-21o6x).
        self.state.composer_visual_layout = Arc::clone(&compositor.composer_visual_layout);

        if let Some(config) = cfg.quiescent_efficiency.clone() {
            self.state.quiescent_efficiency = Some(WindowedQuiescentEfficiencyRunState::new(
                config,
                self.state.effective_mode,
                surface_width,
                surface_height,
                compositor.adapter_info().clone(),
                Instant::now(),
            ));
            tracing::info!(
                settling_ms = tze_hud_telemetry::QUIESCENT_SETTLING_MIN_MS,
                interval_ms = tze_hud_telemetry::QUIESCENT_INTERVAL_MIN_MS,
                "quiescent efficiency measurement armed; waiting for first actual present"
            );
        }

        // ── Wire compositor thread ─────────────────────────────────────────
        // Pre-clone the scene Arc so the compositor thread can lock the scene
        // directly without ever needing to acquire the SharedState lock.
        // This avoids nested-lock inversion: the compositor only ever holds the
        // scene lock; session handlers hold the SharedState lock then the scene lock.
        let compositor_scene = {
            let st = self.state.shared_state.try_lock().expect(
                "windowed runtime: shared_state lock contended at compositor setup — \
                 this should not happen during single-threaded initialisation",
            );
            Arc::clone(&st.scene)
        };
        // Share the ArcSwap handle (not the FramePipeline itself) with the compositor thread.
        let hit_test_snapshot = self.state.pipeline.hit_test_snapshot.clone();
        let pending_input_latency = Arc::clone(&self.state.pending_input_latency);
        let frame_ready_tx = self
            .state
            .frame_ready_tx
            .take()
            .expect("frame_ready_tx already taken");
        // These runtime-owned senders are cloned into each compositor generation.
        // A mode switch joins this thread and calls `resumed()` again against the
        // same connected gRPC session, so taking them would drop later delivery.
        let (frame_presented_tx, degradation_notices, lease_expirations) =
            self.state.compositor_runtime_senders();
        let shutdown = self.state.shutdown.clone();
        let benchmark_failed = self.state.benchmark_failed.clone();
        let terminal_surface_recovery_failed = self.state.terminal_surface_recovery_failed.clone();
        let compositor_wake = self.state.wake.clone();
        let safe_mode_for_compositor = Arc::clone(&self.state.safe_mode_atomic);
        let telemetry_collector = TelemetryCollector::new();
        let surface_for_compositor = window_surface.clone();
        let mut benchmark_state = cfg.benchmark.clone().map(|benchmark| {
            WindowedBenchmarkRunState::new(
                benchmark,
                cfg.window.mode,
                self.state.effective_mode,
                surface_width,
                surface_height,
                cfg.target_fps,
            )
        });

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let compositor_handle = spawn_compositor_thread(
            shutdown.clone(),
            ready_tx,
            move |shutdown_tok, comp_ready| {
                // Signal ready immediately (compositor thread setup is synchronous).
                let _ = comp_ready.send(CompositorReady { ok: true });

                let mut compositor = compositor;
                let mut telemetry = telemetry_collector;
                let degradation_clock_start = Instant::now();
                let degradation_envelope =
                    crate::degradation::DegradationEnvelope::from_effective_fps(
                        degradation_effective_fps,
                    )
                    .expect("validated target cadence must produce a degradation envelope");
                let mut degradation_controller =
                    crate::degradation::DegradationController::with_envelope(
                        crate::degradation::DegradationConfig::default(),
                        degradation_envelope,
                    );

                // Hold a 1 ms OS timer resolution for the whole frame loop so the
                // pacing sleep below does not overshoot the present budget on the
                // default coarse Windows timer (hud-ofe76). Released when the
                // guard drops on any loop-exit path.
                let _frame_timer_guard = FramePacingTimerGuard::acquire();

                let frame_interval =
                    std::time::Duration::from_micros(1_000_000 / cfg.target_fps.max(1) as u64);
                let mut shutdown_rx = shutdown_tok.subscribe();
                // Running total of compositor scene try_lock misses (hud-3qpgv.2).
                // Incremented on each frame-loop Stage 4 try_lock failure and
                // snapshotted into FrameTelemetry::scene_lock_miss_count on every
                // successful (lock-acquired) frame so contention is observable in
                // telemetry.  Plain u64 — accessed only on this thread; no atomics
                // needed on the success path.
                let mut scene_lock_miss_count: u64 = 0;
                // hud-ilivg idle render gate: scene.version of the last frame we
                // actually built+presented. The loop skips the build/encode/present
                // pass when scene.version is unchanged AND no animation is in
                // flight, freeing idle CPU/GPU/streaming budget without dropping any
                // real change. u64::MAX guarantees the very first frame renders.
                let mut last_rendered_scene_version: u64 = u64::MAX;
                // Position-only drag-move mutations advance scene.geometry_epoch
                // (not scene.version), so the present-gate must also repaint when
                // the epoch changes — otherwise a smooth drag would stall on the
                // idle gate. Content caches stay gated on scene.version so the
                // translate never re-primes them (hud-uyhpn).
                let mut last_rendered_geometry_epoch: u64 = u64::MAX;
                // A recovered swapchain needs a fresh submit even if the scene
                // lock is briefly unavailable in the recovery iteration. Keep
                // this debt until a frame actually submits.
                let mut surface_repaint_pending = false;
                // Benchmark progress is acknowledged by the main thread after
                // `SurfaceTexture::present()`, not by a compositor-side attempt.
                // Keep only the latest telemetry for the currently pending
                // swapchain texture because `FrameReadySignal` is latest-wins.
                let mut benchmark_present_count_seen =
                    surface_for_compositor.presented_frame_count();
                let mut pending_benchmark_sample = None;
                crate::diag::diag_write("compositor thread: frame loop STARTED");

                tracing::info!(
                    "compositor thread: starting frame loop at {}fps",
                    cfg.target_fps
                );

                loop {
                    // Checkpoint before inspecting any source of render work.
                    // A producer notification that races with this iteration
                    // advances the generation and prevents the final wait from
                    // parking on stale observations.
                    let wake_checkpoint = compositor_wake.compositor().checkpoint();
                    let mut continue_at_cadence = benchmark_state.is_some();
                    let mut timed_deadline: Option<Deadline> = None;
                    // Check for shutdown.
                    match shutdown_rx.try_recv() {
                        Ok(_) => {
                            tracing::info!("compositor thread: shutdown received");
                            break;
                        }
                        Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                            tracing::info!("compositor thread: shutdown channel closed");
                            break;
                        }
                        Err(_) => {} // Lagged or empty — continue.
                    }
                    if shutdown_tok.is_triggered() {
                        break;
                    }

                    let presented_count = surface_for_compositor.presented_frame_count();
                    if presented_count > benchmark_present_count_seen {
                        benchmark_present_count_seen = presented_count;
                        if let Some(sample) = pending_benchmark_sample.take()
                            && let Some(state) = benchmark_state.as_mut()
                            && state.record(&sample)
                        {
                            let finished = benchmark_state
                                .take()
                                .expect("benchmark_state must still exist when record completes");
                            let emit_path = finished.config.emit_path.clone();
                            match finished.finish() {
                                Ok(()) => {
                                    tracing::info!(
                                        path = %emit_path.display(),
                                        "windowed benchmark artifact written; shutting down"
                                    );
                                    shutdown_tok.trigger(crate::threads::ShutdownReason::Clean);
                                    compositor_wake.notify_main(
                                        crate::idle_efficiency::RuntimeWakeupSource::Shutdown,
                                    );
                                    break;
                                }
                                Err(err) => {
                                    tracing::error!(
                                        error = %err,
                                        path = %emit_path.display(),
                                        "failed to write windowed benchmark artifact"
                                    );
                                    benchmark_failed
                                        .store(true, std::sync::atomic::Ordering::Release);
                                    shutdown_tok.trigger(crate::threads::ShutdownReason::Clean);
                                    compositor_wake.notify_main(
                                        crate::idle_efficiency::RuntimeWakeupSource::Shutdown,
                                    );
                                    break;
                                }
                            }
                        }
                    }

                    let frame_start = Instant::now();
                    // Authoritative degradation workload boundary: all active
                    // compositor work, including resize handling, before Stage 3
                    // through successful Stage 7 completion.
                    let degradation_work_start = Instant::now();

                    // ── Resize check ───────────────────────────────────────
                    // The main thread writes pending_resize_width/height on
                    // WindowEvent::Resized. We detect and apply it here because
                    // the compositor thread owns the wgpu::Device required by
                    // surface.reconfigure().
                    //
                    // Read width last (it was written last by the main thread)
                    // to avoid a torn read: if the main thread is mid-write we
                    // will see the old width and skip this cycle; the resize
                    // will be applied on the next frame instead.
                    let pending_w = surface_for_compositor
                        .pending_resize_width
                        .load(std::sync::atomic::Ordering::Acquire);
                    let pending_h = surface_for_compositor
                        .pending_resize_height
                        .load(std::sync::atomic::Ordering::Acquire);
                    if pending_w > 0 && pending_h > 0 {
                        surface_for_compositor.reconfigure(
                            pending_w,
                            pending_h,
                            &compositor.device,
                        );
                        // Reset pending resize (store 0 to signal "handled").
                        surface_for_compositor
                            .pending_resize_width
                            .store(0, std::sync::atomic::Ordering::Release);
                        surface_for_compositor
                            .pending_resize_height
                            .store(0, std::sync::atomic::Ordering::Release);
                        // Update compositor's cached dimensions.
                        compositor.width = pending_w;
                        compositor.height = pending_h;
                    }

                    // ── Surface recovery check ───────────────────────────
                    // `WindowSurface::acquire_frame` queues real Lost/Outdated
                    // failures. Consume that queue here, on the normal
                    // compositor thread that owns the wgpu Device, before the
                    // next scene build attempts another acquire.
                    let surface_recovery = compositor
                        .attempt_pending_surface_recovery(surface_for_compositor.as_ref());
                    if surface_recovery.is_terminal() {
                        shutdown_after_terminal_surface_recovery(
                            &shutdown_tok,
                            &compositor_wake,
                            terminal_surface_recovery_failed.as_ref(),
                        );
                        break;
                    }
                    // Safe mode changes no scene state, so a flip must itself
                    // force one repaint (the overlay appears / clears); the
                    // transition's render wake brought us here.
                    let safe_mode_flipped = compositor.set_safe_mode_overlay(
                        safe_mode_for_compositor.load(std::sync::atomic::Ordering::Acquire),
                    );
                    surface_repaint_pending = update_surface_repaint_pending(
                        surface_repaint_pending,
                        surface_recovery.reconfigured_surface() || safe_mode_flipped,
                        false,
                    );

                    // ── Stage 3: Mutation Intake ───────────────────────────
                    // (placeholder — real mutations come via gRPC session)

                    // ── Stage 4: Scene Commit + HitTest Snapshot ──────────
                    // Lock the scene directly (never lock SharedState here).
                    // Using try_lock avoids blocking the compositor thread for
                    // too long when a session handler or MCP handler holds the
                    // scene lock momentarily.
                    if let Ok(mut scene) = compositor_scene.try_lock() {
                        // Register runtime-uploaded widget SVG assets before
                        // rendering so newly registered widget types/layers can
                        // be referenced by publish calls immediately.
                        process_pending_widget_svgs(
                            compositor.widget_renderer_mut(),
                            scene.drain_pending_widget_svg_assets(),
                        );

                        // ── Timed-content sweep (invariants 1 and 4) ──────
                        // Expired publications, tiles, and leases MUST be
                        // cleared before the next frame.
                        let terminal_lease_expiries =
                            crate::pipeline::sweep_timed_scene_state(&mut scene);

                        let applied_degradation_level = degradation_controller.level();
                        compositor
                            .set_degradation_policy(degradation_controller.compositor_policy());

                        // ── Per-publication TTL fade-out sweep ───────────
                        // update_publication_animations seeds new state and ticks
                        // existing ones; prune_faded_publications removes any
                        // publications whose 150ms fade-out has completed.
                        compositor.update_publication_animations(&scene);
                        compositor.prune_faded_publications(&mut scene);

                        // ── Commit-time markdown cache prime (hud-380dl) ──
                        // Prime the markdown parse cache here — at scene-commit
                        // time, before the render path executes — so that
                        // render_frame never performs parsing in the frame loop.
                        // This satisfies the "parse-on-commit, zero per-frame
                        // parse cost" contract (Option A, hud-380dl).
                        //
                        // The prime is gated internally on scene.version so it
                        // is a no-op when the scene has not changed.  The measured
                        // cost is attached to the stage4 window so it is visible in
                        // telemetry without inflating the Stage 6 render budget.
                        let markdown_prime_start = Instant::now();
                        compositor.prime_markdown_cache(&scene);
                        let markdown_prime_us = markdown_prime_start.elapsed().as_micros() as u64;

                        // ── Commit-time truncation cache prime (hud-v2z6u) ─
                        // Prime the truncation cache here — at commit time,
                        // after prime_markdown_cache — so that render_frame
                        // never performs shaping in the frame loop.  Gated
                        // internally on scene.version; no-op when unchanged.
                        compositor.prime_truncation_cache(&scene);

                        // ── Local composer drain (hud-ilivg / hud-r3ax6) ──
                        // Drain the local composer echo slot BEFORE the gate.  The
                        // draft echo and the caret blink are driven off out-of-band
                        // state that never bumps scene.version, so the gate would
                        // freeze them unless we both (a) apply a pending keystroke
                        // here — `local_composer` is only populated by this drain,
                        // so it must run before the gate can observe it — and (b)
                        // treat a focused composer (blinking caret) as dirty.
                        // Returns true while a composer is focused/visible or on
                        // the single deactivation frame; false once it is gone, so
                        // the truly-static idle case still skips.
                        let composer_needs_render =
                            compositor.drain_local_composer_and_needs_render();

                        // ── Idle render gate (hud-ilivg) ──────────────────
                        // Build/encode/present only when the scene graph changed
                        // since the last presented frame OR an animation is in
                        // flight OR a focused/just-deactivated composer needs a
                        // frame.  The cheap sweeps above (expiry, publication
                        // tick, prune, cache primes) ALWAYS run, so a fade-out
                        // start or expiry still bumps scene.version and re-arms the
                        // gate; an in-flight eased transition / TTL fade / reveal /
                        // smooth-scroll forces a render so it never freezes. An
                        // explicitly configured benchmark is active fixed-cadence
                        // work and presents every requested sample without changing
                        // the normal runtime idle contract.
                        let needs_render = windowed_frame_needs_render(
                            scene.version != last_rendered_scene_version,
                            scene.geometry_epoch != last_rendered_geometry_epoch,
                            compositor.has_inflight_animation(&scene),
                            composer_needs_render,
                            benchmark_state.is_some(),
                            surface_repaint_pending,
                        );
                        continue_at_cadence |= needs_render;

                        if !needs_render {
                            // Idle: scene unchanged and nothing animating. Release
                            // the lock without building vertices, encoding, or
                            // signalling the main thread to present.
                            let scene_deadline_wall_us = scene
                                .next_publication_expiry_wall_us()
                                .into_iter()
                                .chain(scene.next_timed_content_wall_us())
                                .chain(
                                    scene
                                        .next_lease_deadline_ms(
                                            tze_hud_scene::graph::SceneGraph::DEFAULT_MAX_SUSPENSION_MS,
                                        )
                                        .map(|ms| ms.saturating_mul(1_000)),
                                )
                                .min();
                            drop(scene);
                            publish_lease_expiries(
                                lease_expirations.as_ref(),
                                terminal_lease_expiries,
                            );
                            let now_us = degradation_clock_start.elapsed().as_micros() as u64;
                            if let Some(event) = degradation_controller.record_quiescent_at(now_us)
                            {
                                publish_degradation_transition(
                                    &degradation_controller,
                                    event,
                                    degradation_notices.as_ref(),
                                );
                            }
                            if let Some(recover_at_us) =
                                degradation_controller.next_quiescent_recovery_at_us()
                            {
                                timed_deadline = Some(Deadline::new(
                                    degradation_clock_start
                                        + std::time::Duration::from_micros(recover_at_us),
                                    crate::idle_efficiency::RuntimeWakeupSource::AnimationDeadline,
                                ));
                            }
                            if let Some(wall_us) = scene_deadline_wall_us {
                                let deadline = deadline_from_wall_us(
                                    wall_us,
                                    crate::idle_efficiency::RuntimeWakeupSource::TtlDeadline,
                                );
                                timed_deadline = timed_deadline
                                    .into_iter()
                                    .chain(Some(deadline))
                                    .min_by_key(|candidate| candidate.at);
                            }
                            // A notification counting down to its fade and a
                            // focused composer's caret both change pixels at a
                            // known instant with no other event. Wake exactly
                            // then instead of rendering every frame in between.
                            if let Some(at) = compositor.next_animation_deadline() {
                                timed_deadline = timed_deadline
                                    .into_iter()
                                    .chain(Some(Deadline::new(
                                        at,
                                        crate::idle_efficiency::RuntimeWakeupSource::TtlDeadline,
                                    )))
                                    .min_by_key(|candidate| candidate.at);
                            }
                        } else {
                            // ── Stage 5: Build under the scene lock (hud-uyhpn) ─
                            // Do ALL scene reads (vertex/geometry build, encode
                            // inputs, chrome geometry, hit-region population) here
                            // while holding the lock, producing a self-contained
                            // `WindowedFrameBuild`. Crucially this phase does NOT
                            // touch the swapchain surface — so it never blocks on
                            // vsync while the lock is held.
                            let scene_commit_at = Instant::now();
                            let (surf_w, surf_h) = surface_for_compositor.size();
                            let build = compositor.build_windowed_frame(&mut scene, surf_w, surf_h);
                            // Hit-region refresh + hit-test snapshot are still
                            // computed under the lock, from the geometry we are
                            // about to present (build already populated the
                            // drag-handle hit regions from that same geometry).
                            refresh_interaction_hit_regions_after_render(
                                &compositor,
                                &mut scene,
                                surface_for_compositor.as_ref(),
                            );
                            let new_snap = crate::pipeline::HitTestSnapshot::from_scene(&scene);
                            hit_test_snapshot.store(Arc::new(new_snap));
                            let built_scene_version = scene.version;
                            let built_geometry_epoch = scene.geometry_epoch;
                            // ── Batch-correlated present ack drain (hud-91uu6) ──
                            // gRPC `apply_batch` enqueues accepted batch_ids in
                            // BOTH runtimes, so the windowed present path must drain
                            // this queue every presented frame or it grows unbounded
                            // over a live session. Drain UNDER the lock (cheap: a
                            // VecDeque swap, no I/O) and broadcast `FramePresented`
                            // AFTER the lock is released and the frame is presented
                            // (hud-4va6q) — keeping the send off the hottest
                            // lock-collapse path (hud-uyhpn).
                            let presented_batch_ids = scene.drain_present_ack_batch_ids();
                            // ── DROP the scene lock BEFORE the vsync-blocking
                            // acquire/encode/submit/poll (hud-uyhpn) ────────────
                            // This is the fix: the lock hold now collapses to the
                            // cheap build phase above instead of spanning a full
                            // ~16.6ms refresh interval, so the main-thread
                            // interaction path's `spin_acquire` (12ms budget) no
                            // longer times out and drops drag-move samples.
                            drop(scene);
                            publish_lease_expiries(
                                lease_expirations.as_ref(),
                                terminal_lease_expiries,
                            );

                            // ── Stage 6–7: Present lock-free ──────────────────
                            let present_outcome = compositor.present_windowed_frame_with_outcome(
                                build,
                                surface_for_compositor.as_ref(),
                            );
                            if present_outcome.surface_acquired {
                                compositor_wake.counters().record_surface_acquisition();
                            }
                            if present_outcome.gpu_submitted {
                                compositor_wake.counters().record_gpu_submission();
                            }
                            let compositor_telemetry = present_outcome.telemetry;
                            let frame_submitted = compositor_telemetry.stage7_gpu_submit_us > 0;
                            surface_repaint_pending = update_surface_repaint_pending(
                                surface_repaint_pending,
                                false,
                                frame_submitted,
                            );
                            let degradation_work_time_us =
                                degradation_work_start.elapsed().as_micros() as u64;

                            // ── Signal main thread to present ─────────────────
                            // Per spec §Compositor Thread Ownership (line 55):
                            // "compositor thread MUST signal the main thread via
                            // FrameReadySignal, and only the main thread SHALL call
                            // surface.present()."
                            if frame_submitted {
                                let _ = frame_ready_tx.send(true);
                                compositor_wake.notify_main(
                                    crate::idle_efficiency::RuntimeWakeupSource::FrameReady,
                                );
                                // Advance the idle gate only after a completed
                                // submit. A failed acquire must retry instead of
                                // stranding an unpresented scene as "rendered".
                                last_rendered_scene_version = built_scene_version;
                                last_rendered_geometry_epoch = built_geometry_epoch;
                            }

                            // ── Broadcast FramePresented (hud-4va6q) ──────────
                            // Stage 6–7 are complete, so the batches drained above
                            // are now on screen. Mirror the headless producer
                            // (headless.rs): pair those batch_ids with this frame's
                            // number + present wall-clock and broadcast to gRPC
                            // subscribers, gated on read_telemetry at the session
                            // layer. The send is off the scene lock (already
                            // dropped) and skipped when no subscriber is attached
                            // (compositor-only mode) or no batch was applied.
                            if frame_submitted
                                && let Some(tx) = &frame_presented_tx
                                && !presented_batch_ids.is_empty()
                            {
                                let present_wall_us = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_micros() as u64)
                                    .unwrap_or(0);
                                let event = tze_hud_protocol::proto::FramePresented {
                                    frame_number: compositor_telemetry.frame_number,
                                    present_wall_us,
                                    // 16-byte big-endian UUID, matching the
                                    // scene_id_to_bytes / bytes_to_scene_id wire
                                    // contract used for MutationBatch.batch_id.
                                    batch_ids: presented_batch_ids
                                        .iter()
                                        .map(|id| id.as_uuid().as_bytes().to_vec())
                                        .collect(),
                                };
                                // Broadcast send errors only when there are no
                                // subscribers — not an error for a droppable
                                // state-stream present ack.
                                let _ = tx.send(event);
                            }

                            // Telemetry emit (Stage 8)
                            let mut telem = tze_hud_telemetry::FrameTelemetry::new(
                                compositor_telemetry.frame_number,
                            );
                            telem.frame_time_us = frame_start.elapsed().as_micros() as u64;
                            telem.degradation_work_time_us = degradation_work_time_us;
                            telem.degradation_level = applied_degradation_level.as_u8();
                            telem.stage6_render_encode_us =
                                compositor_telemetry.stage6_render_encode_us;
                            telem.stage7_gpu_submit_us = compositor_telemetry.stage7_gpu_submit_us;
                            telem.tile_count = compositor_telemetry.tile_count;
                            // Propagate commit-time markdown prime cost (hud-380dl).
                            // Non-zero only on frames where scene.version changed;
                            // zero on steady-state frames (cache hit, no parse work).
                            telem.markdown_prime_us = markdown_prime_us;
                            // Snapshot the cumulative scene-lock miss count so this
                            // frame's telemetry record carries contention history
                            // (hud-3qpgv.2). No extra cost on the success path: plain
                            // u64 read, no atomics, no cross-thread access.
                            telem.scene_lock_miss_count = scene_lock_miss_count;
                            if let Some((local_ack_us, scene_commit_us, next_present_us)) =
                                drain_pending_input_latency(
                                    &pending_input_latency,
                                    scene_commit_at,
                                    Instant::now(),
                                )
                            {
                                telem.input_to_local_ack_us = local_ack_us;
                                telem.input_to_scene_commit_us = scene_commit_us;
                                telem.input_to_next_present_us = next_present_us;
                            }
                            telemetry.record(telem);

                            let completed_at_us =
                                degradation_clock_start.elapsed().as_micros() as u64;
                            if frame_submitted
                                && let Some(event) = degradation_controller
                                    .record_frame_at(degradation_work_time_us, completed_at_us)
                            {
                                publish_degradation_transition(
                                    &degradation_controller,
                                    event,
                                    degradation_notices.as_ref(),
                                );
                            }

                            if frame_submitted && benchmark_state.is_some() {
                                pending_benchmark_sample = telemetry.records().last().cloned();
                            }
                        }
                    } else {
                        // Stage 4 try_lock missed: the scene lock was held by a
                        // concurrent gRPC/MCP handler.  Record the miss so it is
                        // visible in the next successful frame's telemetry
                        // (hud-3qpgv.2).  This branch has zero cost on the
                        // success path.
                        scene_lock_miss_count = scene_lock_miss_count.saturating_add(1);
                        // Do not turn a temporary lock miss into a fixed frame
                        // cadence. One coalesced waiter wakes the compositor
                        // exactly when the held scene becomes available again.
                        let availability_scene = Arc::clone(&compositor_scene);
                        compositor_wake.schedule_compositor_after_availability(move || {
                            drop(availability_scene.blocking_lock());
                        });
                    }

                    // Benchmark no-progress watchdog (hud-gcn01): if the benchmark
                    // is active and no frame has been rendered within the timeout,
                    // emit a partial/diagnostic artifact and exit non-zero.  This
                    // catches the Windows fullscreen hang where redraw callbacks
                    // never fire for a non-foreground window, preventing a silent
                    // infinite spin that never reaches --benchmark-frames.
                    let benchmark_stalled = benchmark_state
                        .as_ref()
                        .is_some_and(|s| s.is_stalled(BENCHMARK_NO_PROGRESS_TIMEOUT));
                    if benchmark_stalled {
                        let finished = benchmark_state
                            .take()
                            .expect("stalled implies benchmark_state is Some");
                        let emit_path = finished.config.emit_path.clone();
                        match finished.emit_watchdog_abort("no-progress timeout") {
                            Ok(()) => tracing::warn!(
                                path = %emit_path.display(),
                                timeout_secs = BENCHMARK_NO_PROGRESS_TIMEOUT.as_secs(),
                                "benchmark watchdog: no-progress timeout — \
                                 partial result emitted; exiting non-zero"
                            ),
                            Err(err) => tracing::error!(
                                error = %err,
                                path = %emit_path.display(),
                                "benchmark watchdog: failed to emit partial result"
                            ),
                        }
                        benchmark_failed.store(true, std::sync::atomic::Ordering::Release);
                        shutdown_tok.trigger(crate::threads::ShutdownReason::Clean);
                        compositor_wake
                            .notify_main(crate::idle_efficiency::RuntimeWakeupSource::Shutdown);
                        break;
                    }

                    let cadence_deadline = continue_at_cadence.then(|| {
                        Deadline::new(
                            frame_start + frame_interval,
                            crate::idle_efficiency::RuntimeWakeupSource::AnimationDeadline,
                        )
                    });
                    let next_deadline = cadence_deadline
                        .into_iter()
                        .chain(timed_deadline)
                        .min_by_key(|candidate| candidate.at);
                    let observed = compositor_wake
                        .compositor()
                        .wait(wake_checkpoint, next_deadline);
                    compositor_wake
                        .counters()
                        .record_compositor_wakeup(observed.source);
                }

                tracing::info!("compositor thread: frame loop exited");
                crate::diag::diag_write(
                    "compositor thread: frame loop EXITED — no more frames will present (HB-2)",
                );
            },
        );

        self.state.compositor_handle = Some(compositor_handle);

        // Wait for the compositor thread to signal ready (with timeout).
        let tmp_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("startup runtime 2");
        let compositor_ok = tmp_rt
            .block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx).await
            })
            .ok()
            .and_then(|r| r.ok())
            .map(|r| r.ok)
            .unwrap_or(false);

        if !compositor_ok {
            tracing::warn!("compositor thread did not signal ready in time");
        } else {
            tracing::info!("windowed runtime initialised successfully");
            tracing::info!(target: "tze_hud::resident_accounting", snapshot = %self.state.runtime_context.resident_accounting_snapshot(), "windowed runtime resident accounting initialised");
        }

        // Request first frame.
        window.request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let wake_source = main_work_source_for_window_event(&event);
        match event {
            // ── Close ──────────────────────────────────────────────────────
            WindowEvent::CloseRequested => {
                tracing::info!("main thread: window close requested");
                self.state
                    .shutdown
                    .trigger(crate::threads::ShutdownReason::Clean);
                event_loop.exit();
            }

            // ── Resize ─────────────────────────────────────────────────────
            WindowEvent::Resized(physical_size) => {
                if let Some(source) = wake_source {
                    self.state.wake.mark_main_work_pending(source);
                }
                if physical_size.width > 0 && physical_size.height > 0 {
                    self.state.config.window.width = physical_size.width;
                    self.state.config.window.height = physical_size.height;
                    if let Ok(state) = self.state.shared_state.try_lock() {
                        if let Ok(mut scene) = state.scene.try_lock() {
                            sync_scene_display_area(
                                &mut scene,
                                physical_size.width,
                                physical_size.height,
                            );
                        }
                    }
                }
                if let Some(surface) = &self.state.window_surface {
                    tracing::info!(
                        width = physical_size.width,
                        height = physical_size.height,
                        "main thread: window resized — signalling compositor for reconfiguration"
                    );
                    // Signal the compositor thread to reconfigure the surface.
                    // The compositor thread owns the wgpu::Device and is the
                    // only thread that can safely call surface.configure().
                    //
                    // We write the new dimensions atomically. The compositor
                    // thread reads `pending_resize_width/height` at the start of
                    // each frame cycle, calls `surface.reconfigure()` when
                    // non-zero, and resets both fields to 0.
                    //
                    // Write height first so the compositor never sees a
                    // partially-updated pair (width updated, height stale).
                    surface
                        .pending_resize_height
                        .store(physical_size.height, std::sync::atomic::Ordering::Release);
                    surface
                        .pending_resize_width
                        .store(physical_size.width, std::sync::atomic::Ordering::Release);
                }
            }

            // ── Pointer: cursor moved ──────────────────────────────────────
            // Stage 1: Drain OS input event → InputEvent ring buffer.
            // Stage 2: Apply local feedback.
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(source) = wake_source {
                    self.state.wake.mark_main_work_pending(source);
                }
                self.state.cursor_x = position.x as f32;
                self.state.cursor_y = position.y as f32;
                self.state.cursor_left_window = false;

                if self.synthesize_left_release_if_physically_up() {
                    return;
                }
                self.enqueue_pointer_event(PointerEventKind::Move);
            }

            // Pointer left the window: hide any hover close button.
            WindowEvent::CursorLeft { .. } => {
                if let Some(source) = wake_source {
                    self.state.wake.mark_main_work_pending(source);
                }
                self.state.cursor_left_window = true;
            }

            // ── Pointer: button press/release ──────────────────────────────
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(source) = wake_source {
                    self.state.wake.mark_main_work_pending(source);
                }
                if button == MouseButton::Left {
                    let kind = match state {
                        ElementState::Pressed => PointerEventKind::Down,
                        ElementState::Released => PointerEventKind::Up,
                    };
                    if state == ElementState::Pressed {
                        if self.state.left_button_down {
                            self.enqueue_pointer_event(PointerEventKind::Up);
                            end_os_mouse_capture();
                        }
                        self.state.left_button_down = true;
                        if let Some(window) = &self.state.window {
                            focus_window_for_text_input(window);
                            begin_os_mouse_capture(window);
                        }
                        self.update_overlay_cursor_hittest();
                    }
                    // If the context menu is showing and this is a left-press,
                    // check if it lands on the Reset button.  If so, trigger
                    // the reset; otherwise dismiss the menu (click-outside).
                    if state == ElementState::Released {
                        self.handle_left_click_with_context_menu();
                    }
                    self.enqueue_pointer_event(kind);
                    if state == ElementState::Released {
                        self.state.left_button_down = false;
                        end_os_mouse_capture();
                        self.update_overlay_cursor_hittest();
                    }
                } else if button == MouseButton::Right && state == ElementState::Released {
                    // Right-click: show context menu if cursor is on a drag handle.
                    self.handle_right_click_on_drag_handle();
                }
            }

            // ── Pointer: wheel scroll ────────────────────────────────────────
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(source) = wake_source {
                    self.state.wake.mark_main_work_pending(source);
                }
                let (delta_x, delta_y) = normalize_mouse_wheel_delta(&delta);
                self.enqueue_scroll_event(delta_x, delta_y);
            }

            // ── Modifiers ─────────────────────────────────────────────────
            WindowEvent::ModifiersChanged(mods) => {
                self.state.modifiers = mods.state();
            }

            // ── Keyboard ──────────────────────────────────────────────────
            // Stage 1: Drain keyboard events into the input ring buffer.
            // Map winit keyboard events to InputEventKind::KeyPress / KeyRelease.
            WindowEvent::KeyboardInput { event, .. } => {
                // ── Monitor cycling: Ctrl+Shift+F9 (next) / Ctrl+Shift+F8 (prev)
                if event.state == ElementState::Pressed && !event.repeat {
                    use winit::keyboard::{KeyCode, PhysicalKey};
                    let mods = self.state.modifiers;
                    let ctrl_shift = mods.control_key()
                        && mods.shift_key()
                        && !mods.alt_key()
                        && !mods.super_key();
                    if ctrl_shift {
                        match event.physical_key {
                            PhysicalKey::Code(KeyCode::F9) => {
                                self.cycle_monitor(event_loop, 1);
                                self.state
                                    .consumed_shell_shortcut_keydowns
                                    .insert("F9".to_string());
                                return;
                            }
                            PhysicalKey::Code(KeyCode::F8) => {
                                self.cycle_monitor(event_loop, -1);
                                self.state
                                    .consumed_shell_shortcut_keydowns
                                    .insert("F8".to_string());
                                return;
                            }
                            _ => {}
                        }
                    }
                }

                let matching_shell_release_is_consumed = event.state == ElementState::Released
                    && self
                        .state
                        .consumed_shell_shortcut_keydowns
                        .contains(&physical_key_to_key_code_str(&event.physical_key));
                if !matching_shell_release_is_consumed {
                    if let Some(source) = wake_source {
                        self.state.wake.mark_main_work_pending(source);
                    }
                }

                // Extract a u32 key code from the physical key for the channel type.
                let key_u32 = physical_key_to_u32(&event.physical_key);
                let input_event = InputEvent {
                    timestamp_ns: nanoseconds_since_start(),
                    kind: if event.state == ElementState::Pressed {
                        InputEventKind::KeyPress { key: key_u32 }
                    } else {
                        InputEventKind::KeyRelease { key: key_u32 }
                    },
                };
                enqueue_input(&self.state.input_ring, input_event);

                // ── Keyboard → KeyboardProcessor drain (Stage 2) ─────────────
                // Translate the raw OS event to a typed KeyboardDispatch using the
                // current focus state, then dispatch to the owning agent session.
                //
                // The physical_key → DOM-style key_code string and the logical_key
                // → DOM-style key string are extracted here for RFC 0004 §7.4
                // compatibility. Only press and repeat events are forwarded (not
                // key-release events for now — release delivery is a follow-up).
                let key_code_str = physical_key_to_key_code_str(&event.physical_key);
                let logical_key_str = logical_key_to_str(&event.logical_key);
                let mods = winit_mods_to_keyboard_modifiers(self.state.modifiers);
                let timestamp_mono_us = tze_hud_scene::MonoUs(nanoseconds_since_start() / 1_000);
                let paste_shortcut_pressed = event.state == ElementState::Pressed
                    && !event.repeat
                    && (mods.ctrl || mods.meta)
                    && !mods.alt
                    && logical_key_str.eq_ignore_ascii_case("v");

                if event.state == ElementState::Pressed || event.repeat {
                    let raw = RawKeyDownEvent {
                        key_code: key_code_str,
                        key: logical_key_str.clone(),
                        modifiers: mods,
                        repeat: event.repeat,
                        timestamp_mono_us,
                    };
                    self.dispatch_key_down_event(&raw);
                } else if event.state == ElementState::Released {
                    let raw = RawKeyUpEvent {
                        key_code: physical_key_to_key_code_str(&event.physical_key),
                        key: logical_key_str,
                        modifiers: mods,
                        timestamp_mono_us,
                    };
                    self.dispatch_key_up_event(&raw);
                }

                if paste_shortcut_pressed {
                    if let Some(text) = read_windows_clipboard_text() {
                        let raw_char = RawCharacterEvent {
                            character: text,
                            timestamp_mono_us,
                        };
                        self.dispatch_character_event(&raw_char);
                    }
                }

                // ── Character input via Key::Character (non-IME path) ────────
                // When the logical key carries a printable character, produce a
                // RawCharacterEvent so the KeyboardProcessor character path is
                // also exercised (handles basic ASCII without an IME active).
                // IME commit characters arrive via WindowEvent::Ime below.
                if event.state == ElementState::Pressed && !mods.ctrl && !mods.meta && !mods.alt {
                    use winit::keyboard::Key;
                    if let Key::Character(ch) = event.logical_key.as_ref() {
                        let raw_char = RawCharacterEvent {
                            character: ch.to_string(),
                            timestamp_mono_us,
                        };
                        self.dispatch_character_event(&raw_char);
                    }
                }
            }

            // ── IME commit: post-composition character delivery ───────────────
            // `WindowEvent::Ime(Ime::Commit(text))` is the canonical path for
            // IME-composed characters (CJK, accented inputs, etc.). In v1 the
            // commit text is forwarded as a RawCharacterEvent so agents receive
            // CharacterEvent payloads regardless of input method.
            //
            // Preedit events (Ime::Preedit) are v1-reserved and not forwarded.
            WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                if let Some(source) = wake_source {
                    self.state.wake.mark_main_work_pending(source);
                }
                let timestamp_mono_us = tze_hud_scene::MonoUs(nanoseconds_since_start() / 1_000);
                let raw_char = RawCharacterEvent {
                    character: text.clone(),
                    timestamp_mono_us,
                };
                self.dispatch_character_event(&raw_char);
            }

            // ── Redraw ────────────────────────────────────────────────────
            WindowEvent::RedrawRequested => {
                // OS-driven repaint (expose / resize / the initial redraw request
                // in `resumed`).  Per-frame bookkeeping and the present poll now
                // live in `about_to_wait` (hud-ilivg); here we only service the
                // present so an OS-requested repaint shows the latest compositor
                // frame.  The handler no longer self-perpetuates a redraw: the
                // compositor render gate decides when new frames exist and
                // `about_to_wait` polls for them every iteration, so an idle scene
                // no longer drives a continuous 60 Hz redraw/present cycle.
                self.maybe_present_frame();
            }

            _ => {}
        }
    }
}

/// Only OS events that can produce compositor-visible main-thread work own a
/// post-settle wake generation. Present polling, modifier bookkeeping, and
/// shutdown are not scene mutations and must not manufacture another frame.
fn main_work_source_for_window_event(
    event: &WindowEvent,
) -> Option<crate::idle_efficiency::RuntimeWakeupSource> {
    use crate::idle_efficiency::RuntimeWakeupSource;

    match event {
        WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
            Some(RuntimeWakeupSource::Resize)
        }
        WindowEvent::CursorMoved { .. }
        | WindowEvent::CursorLeft { .. }
        | WindowEvent::MouseInput {
            button: MouseButton::Left,
            ..
        }
        | WindowEvent::MouseInput {
            button: MouseButton::Right,
            state: ElementState::Released,
            ..
        }
        | WindowEvent::MouseWheel { .. }
        | WindowEvent::KeyboardInput { .. }
        | WindowEvent::Ime(winit::event::Ime::Commit(_)) => Some(RuntimeWakeupSource::SceneChange),
        _ => None,
    }
}

/// Entry point for the windowed runtime.
///
/// Owns all windowed runtime state. Call `run()` to hand control to the
/// winit event loop (this call blocks until the window is closed).
pub struct WindowedRuntime {
    config: WindowedConfig,
}

impl WindowedRuntime {
    /// Create a new `WindowedRuntime` with the given config.
    pub fn new(config: WindowedConfig) -> Self {
        Self { config }
    }

    /// Run the windowed runtime event loop.
    ///
    /// This is a **blocking call** that runs on the main thread until the window
    /// is closed or a shutdown signal is received. It creates the winit event
    /// loop, initialises the window + compositor + surface, spawns the compositor
    /// thread, and enters the winit event loop.
    ///
    /// Per spec §Main Thread Responsibilities (line 33): "The main thread MUST
    /// run the winit event loop."
    ///
    /// # Errors
    ///
    /// Returns an error if the winit event loop or window creation fails.
    pub fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        let cfg = self.config;
        let runtime_context: SharedRuntimeContext = build_runtime_context(&cfg);

        let event_loop = EventLoop::<RuntimeWakeEvent>::with_user_event().build()?;
        event_loop.set_control_flow(ControlFlow::Wait);
        let wake = WindowedWake::new(event_loop.create_proxy());
        let render_wake = wake.render_notifier();
        let portal_ingress_wake = wake.main_work_notifier();

        let effective_mode = cfg.window.mode;

        // Build shared state (scene + sessions).
        let width = cfg.window.width as f32;
        let height = cfg.window.height as f32;
        // Parse the raw config once here so we can use it for both widget
        // registry initialization and the RuntimeContext build. Failure is
        // non-fatal — widget startup will just leave the registry empty.
        let raw_config_for_startup: Option<tze_hud_config::raw::RawConfig> = cfg
            .config_toml
            .as_deref()
            .and_then(|toml| toml::from_str(toml).ok());

        let mut pending_widget_svgs: Vec<crate::widget_startup::WidgetSvgAsset> = Vec::new();
        let (
            shared_scene,
            startup_compositor_tokens,
            runtime_widget_store,
            startup_element_store,
            startup_element_store_path,
        ) = {
            let mut scene = SceneGraph::new(width, height);

            // Resolve config file parent directory for path resolution.
            let config_parent_buf: Option<std::path::PathBuf> = cfg
                .config_file_path
                .as_deref()
                .and_then(|p| std::path::Path::new(p).parent().map(|d| d.to_path_buf()));

            let runtime_widget_store = if let Some(raw) = &raw_config_for_startup {
                let resolved =
                    resolve_runtime_widget_asset_store(raw, config_parent_buf.as_deref()).map_err(
                        |e| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                format!("runtime widget asset store config invalid: {}", e.hint),
                            )
                        },
                    )?;
                Some(RuntimeWidgetStore::open(RuntimeWidgetStoreConfig {
                    store_path: resolved.store_path,
                    max_total_bytes: resolved.max_total_bytes,
                    max_agent_bytes: resolved.max_agent_bytes,
                })?)
            } else {
                None
            };

            // Scene startup: design tokens, config tabs, widget bundles, and
            // token-derived zone rendering policies.
            let compositor_tokens = if let Some(raw) = &raw_config_for_startup {
                let startup_result =
                    run_scene_startup(raw, config_parent_buf.as_deref(), &mut scene);
                // Stash SVG assets for compositor registration after init_widget_renderer.
                pending_widget_svgs = startup_result.widget_svg_assets;
                startup_result.global_tokens
            } else {
                // No config provided — bootstrap with canonical zone defaults (no token derivation).
                scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
                std::collections::HashMap::new()
            };

            if std::env::var("TZE_HUD_SIM_SUBTITLES").as_deref() == Ok("1") {
                let samples = [
                    "Subtitle demo: systems online.",
                    "Subtitle demo: compositor stable.",
                    "Subtitle demo: overlay path verified.",
                ];
                for line in samples {
                    if let Err(e) = scene.publish_to_zone(
                        "subtitle",
                        ZoneContent::StreamText(line.to_string()),
                        "hud-user-sim",
                        None,
                        None,
                        None,
                    ) {
                        tracing::warn!(error = %e, "failed to seed subtitle demo line");
                    }
                }
            }
            let element_store_bootstrap = bootstrap_scene_element_store(&mut scene);
            (
                Arc::new(Mutex::new(scene)),
                compositor_tokens,
                runtime_widget_store,
                element_store_bootstrap.store,
                element_store_bootstrap.path,
            )
        };
        let sessions = tze_hud_protocol::session::SessionRegistry::new();
        let (input_capture_tx, input_capture_rx) = tokio::sync::mpsc::unbounded_channel();
        let safe_mode_atomic = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Lock-free mirror of `scene.active_tab` for the winit event thread's
        // keyboard-dispatch path (hud-dwcr7).  Held both by `SharedState` (the
        // writer side) and cloned into the `WinitApp` (the lock-free reader
        // side) so composer echo never try_locks the scene mutex.
        let active_tab_mirror = Arc::new(std::sync::Mutex::new(None));
        let chrome_state = Arc::new(std::sync::RwLock::new(crate::shell::ChromeState::new()));
        let resident_limits = runtime_context.resident_store_limits();
        let shared_state = Arc::new(Mutex::new(SharedState {
            scene: Arc::clone(&shared_scene),
            sessions,
            resource_store: tze_hud_resource::ResourceStore::new_with_resident_ledger(
                tze_hud_resource::ResourceStoreConfig {
                    max_total_texture_bytes: resident_limits.resource_bytes,
                    ..tze_hud_resource::ResourceStoreConfig::default()
                },
                runtime_context.resident_ledger.clone(),
            ),
            widget_asset_store:
                tze_hud_protocol::session::WidgetAssetStore::new_with_limits_and_resident_ledger(
                    resident_limits.widget_source_bytes,
                    resident_limits.widget_namespace_bytes,
                    runtime_context.resident_ledger.clone(),
                ),
            runtime_widget_store: runtime_widget_store.clone(),
            element_store: startup_element_store,
            element_store_path: Some(startup_element_store_path),
            safe_mode_atomic: Arc::clone(&safe_mode_atomic),
            active_tab_mirror: Arc::clone(&active_tab_mirror),
            token_store: TokenStore::new(),
            freeze_active: false,
            input_capture_tx: Some(input_capture_tx),
            input_capture_wake: wake.main_work_notifier(),
            tile_placement: tze_hud_config::tile_placement_from_tokens(&startup_compositor_tokens),
        }));

        let (frame_ready_tx, frame_ready_rx) = frame_ready_channel();
        let input_ring = Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::with_capacity(INPUT_EVENT_CAPACITY),
        ));
        let pending_input_latency = Arc::new(StdMutex::new(VecDeque::new()));
        let shutdown = ShutdownToken::new();
        // `--uninstall` / an upgrading installer ask this instance to quit.
        {
            let shutdown = shutdown.clone();
            let wake = wake.clone();
            crate::operator::install::spawn_quit_listener(move || {
                shutdown.trigger(crate::threads::ShutdownReason::Clean);
                wake.notify_main(crate::idle_efficiency::RuntimeWakeupSource::Shutdown);
            });
        }

        // ── Network runtime + gRPC + MCP HTTP servers ──────────────────────────
        // Spawn the Tokio multi-thread runtime for all network tasks (gRPC, MCP).
        // The runtime is created before the winit event loop so that network
        // services are available immediately after the process starts.
        //
        // Per spec §Thread Model (line 15): "Network thread(s) — Tokio multi-thread
        // runtime for gRPC server, MCP bridge, session management."
        //
        // gRPC server is disabled when grpc_port == 0 (per WindowedConfig docs).
        // gRPC and MCP listen on loopback plus local Tailscale addresses only.
        let (
            mut network_rt,
            mut network_handles,
            element_repositioned_tx,
            input_event_tx,
            frame_presented_tx,
            degradation_notices,
            lease_expirations,
            grpc_bound_addrs,
        ) = start_network_services_with_render_wake(
            cfg.grpc_port,
            Arc::clone(&cfg.agents),
            shared_state.clone(),
            Arc::clone(&runtime_context),
            render_wake.clone(),
        )?;

        // ── MCP HTTP server ────────────────────────────────────────────────────
        //
        // Scene coherence: the MCP server and gRPC session server share the
        // same `Arc<Mutex<SceneGraph>>` (`shared_scene`).  Mutations applied
        // over gRPC are immediately visible to MCP queries and vice versa.
        // Portal-op channel: bridges MCP async task → winit event-loop thread
        // (hud-bq0gl.2).  When the MCP server starts successfully the sender is
        // moved into it; the receiver is stored in `WindowedRuntimeState` and
        // drained via `drain_portal_ops` on each `about_to_wait` iteration.
        // If MCP is disabled or fails to bind, both halves are dropped and
        // `portal_op_rx` in state is `None`.
        // Only create the channel when MCP is enabled. If we created it
        // unconditionally and MCP is disabled, the sender half would be dropped
        // immediately while the receiver lived on in `WindowedRuntimeState`,
        // making the first `drain_portal_ops` tick observe `Disconnected` and
        // log a misleading "MCP portal tools will no longer function" warning.
        // Bound MCP address for the startup banner (hud-ylwqc). Set only when the
        // MCP listener actually binds, so the banner never advertises a dead port.
        let mut mcp_bound_addrs: Vec<std::net::SocketAddr> = Vec::new();
        let (mut portal_op_tx_opt, mut portal_op_rx_opt): (
            Option<tokio::sync::mpsc::UnboundedSender<tze_hud_mcp::portal_op::PortalOp>>,
            Option<tokio::sync::mpsc::UnboundedReceiver<tze_hud_mcp::portal_op::PortalOp>>,
        ) = if cfg.mcp_port > 0 {
            let (tx, rx) =
                tokio::sync::mpsc::unbounded_channel::<tze_hud_mcp::portal_op::PortalOp>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        if cfg.mcp_port > 0 {
            // Ensure we have a network runtime to host the MCP task. If gRPC
            // was disabled (grpc_port == 0), network_rt is None and we need
            // to create a fresh one for MCP.
            if network_rt.is_none() {
                match NetworkRuntime::new() {
                    Ok(rt) => {
                        tracing::info!("MCP: created dedicated network runtime (gRPC disabled)");
                        network_rt = Some(rt);
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "failed to create network runtime for MCP; MCP will not be available"
                        );
                    }
                }
            }

            if let Some(ref rt) = network_rt {
                // Same addresses as gRPC: loopback plus local Tailscale.
                let mcp_config = McpServerConfig {
                    bind_addrs: crate::net_addrs::listen_addrs(
                        &crate::net_addrs::local_ips(),
                        cfg.mcp_port,
                    ),
                    late_tailnet_port: Some(cfg.mcp_port),
                    agents: Arc::clone(&cfg.agents),
                    presents: Some(Arc::clone(wake.counters())),
                };
                let mcp_shutdown = shutdown.clone();
                match rt.rt.block_on(start_mcp_http_server_with_render_wake(
                    Arc::clone(&shared_scene),
                    mcp_config,
                    mcp_shutdown,
                    portal_op_tx_opt.take(),
                    render_wake.clone(),
                    portal_ingress_wake.clone(),
                    Arc::clone(&safe_mode_atomic),
                )) {
                    Ok((handle, local_addrs)) => {
                        network_handles.push(handle);
                        mcp_bound_addrs = local_addrs;
                        tracing::info!(
                            mcp_port = cfg.mcp_port,
                            "MCP HTTP server started on network runtime"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            mcp_port = cfg.mcp_port,
                            error = %e,
                            "failed to bind MCP HTTP server; runtime will continue without MCP"
                        );
                    }
                }
            }
        } else {
            tracing::info!("MCP HTTP server disabled (mcp_port = 0)");
        }

        // ── Non-secret startup banner (hud-ylwqc) ──────────────────────────────
        // Print a minimal, self-describing banner to stdout *unconditionally*.
        // The runtime's tracing subscriber is gated on `TZE_HUD_LOG`, so with it
        // unset the process is otherwise silent and a fresh operator cannot tell
        // where it is listening or how to attach. `println!` (not `tracing`) is
        // deliberate for exactly that reason. The banner carries only bound
        // addresses and an attach hint — never the PSK or any credential (the
        // helper cannot access secrets; see `render_startup_banner`).
        //
        // Both `grpc_bound_addrs` and `mcp_bound_addrs` are genuine bound
        // `local_addr`s (empty when the service is disabled), so the banner
        // never advertises an endpoint that did not actually come up.
        println!(
            "{}",
            render_startup_banner(&grpc_bound_addrs, &mcp_bound_addrs)
        );

        // ── Safe-mode global hotkey (hud-jm8nq.10) ─────────────────────────────
        // A dedicated Windows thread owns the RegisterHotKey registration and
        // signals the bridge task; the unfocused click-through overlay never
        // receives the keystroke itself. Needs the network runtime (the bridge
        // is async); without one there is nothing to suspend anyway.
        #[cfg(target_os = "windows")]
        if let Some(ref rt) = network_rt {
            let toggle_tx = safe_mode_toggle::spawn_safe_mode_toggle_bridge(
                rt.rt.handle(),
                Arc::clone(&shared_state),
                Arc::clone(&chrome_state),
                render_wake.clone(),
                shutdown.clone(),
            );
            global_hotkey::spawn_global_hotkey(runtime_context.safe_mode_hotkey, toggle_tx);
        }

        let portal_projection_driver =
            crate::portal_projection_driver::InProcessPortalDriver::new();

        let app_state = WindowedRuntimeState {
            config: cfg,
            wake,
            scheduled_main_deadline: None,
            compositor_handle: None,
            network_rt,
            network_handles,
            runtime_context,
            _runtime_widget_store: runtime_widget_store,
            shared_state,
            safe_mode_atomic,
            active_tab_mirror,
            chrome_state,
            input_ring,
            pending_input_latency,
            frame_ready_rx,
            frame_ready_tx: Some(frame_ready_tx),
            frame_presented_tx,
            degradation_notices,
            lease_expirations,
            compositor: None,
            window_surface: None,
            input_processor: InputProcessor::new(),
            input_capture_rx,
            pending_input_capture_commands: std::collections::VecDeque::new(),
            focus_manager: FocusManager::new(),
            keyboard_processor: KeyboardProcessor::new(),
            telemetry: TelemetryCollector::new(),
            pipeline: FramePipeline::new(),
            shutdown,
            benchmark_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            terminal_surface_recovery_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            quiescent_efficiency: None,
            cursor_x: 0.0,
            cursor_y: 0.0,
            left_button_down: false,
            cursor_tracker: CursorIconTracker::new(),
            window: None,
            effective_mode,
            hit_regions: Vec::new(),
            overlay_capturing: false,
            static_hit_regions: Vec::new(),
            widget_hover_trackers: std::collections::HashMap::new(),
            pending_mode_switch: None,
            pending_widget_svgs,
            modifiers: winit::keyboard::ModifiersState::empty(),
            current_monitor_index: 0,
            global_tokens: startup_compositor_tokens,
            element_repositioned_tx,
            input_event_tx,
            pending_blur_delivery_context: None,
            composer_pointer_drag_anchor: None,
            portal_resize_states: std::collections::HashMap::new(),
            consumed_portal_resize_keydowns: std::collections::HashSet::new(),
            consumed_shell_shortcut_keydowns: std::collections::HashSet::new(),
            keyboard_activation_nodes: std::collections::HashMap::new(),
            consumed_command_keydowns: std::collections::HashSet::new(),
            // Placeholder; replaced in resumed() with the Arc cloned from the
            // compositor.  Separate Arc so it works before compositor is created.
            local_composer_state: Arc::new(StdMutex::new(None)),
            viewer_echo_queue: Arc::new(StdMutex::new(Vec::new())),
            input_history_seed_states: std::collections::HashMap::new(),
            focus_ring_owner_state: Arc::new(StdMutex::new(None)),
            // Placeholder; replaced in resumed() with the compositor's Arc (hud-wgiys).
            resize_grip_hover_state: Arc::new(StdMutex::new(None)),
            tile_close_hover_state: Arc::new(StdMutex::new(None)),
            cursor_left_window: false,
            // Placeholder; replaced in resumed() with the compositor's Arc (hud-21o6x).
            composer_visual_layout: Arc::new(StdMutex::new(None)),
            portal_projection_driver,
            portal_op_rx: portal_op_rx_opt.take(),
            pending_keyboard_events: VecDeque::new(),
            interaction_feedback_lock_misses: std::sync::atomic::AtomicU64::new(0),
        };

        let mut app = WinitApp { state: app_state };

        // Create winit event loop and run.
        // Per spec §Main Thread Responsibilities: winit event loop MUST run on main thread.
        event_loop.run_app(&mut app)?;

        // ── Post-event-loop cleanup ───────────────────────────────────────────

        // Ensure shutdown is triggered before draining threads/tasks.
        // WindowEvent::CloseRequested already triggers it in the normal path,
        // but other exit paths (OS SIGTERM, explicit exit_loop) may not.
        if !app.state.shutdown.is_triggered() {
            app.state
                .shutdown
                .trigger(crate::threads::ShutdownReason::Clean);
        }
        app.state
            .wake
            .notify_compositor(crate::idle_efficiency::RuntimeWakeupSource::Shutdown);

        // Abort all spawned network task handles (gRPC, MCP) so they do not
        // linger past process exit.  The shutdown token already signals tasks
        // to exit gracefully; abort() is a fallback for tasks that ignore it.
        for handle in app.state.network_handles.drain(..) {
            handle.abort();
        }

        // Cleanly join the compositor thread after the event loop exits.
        //
        // Without this, the compositor thread is detached (JoinHandle drop ≠
        // join) and may still be running GPU work during process teardown,
        // leading to device-lost errors or use-after-free in wgpu internals.
        //
        // The shutdown token was already triggered via CloseRequested
        // (WindowEvent::CloseRequested calls shutdown.trigger()), so the
        // compositor frame loop should exit promptly.
        if let Some(handle) = app.state.compositor_handle.take() {
            tracing::info!("waiting for compositor thread to exit...");
            if let Err(e) = handle.join() {
                tracing::error!("compositor thread panicked: {e:?}");
            } else {
                tracing::info!("compositor thread exited cleanly");
            }
        }

        // Shutdown the network runtime (drains gRPC + MCP tasks).
        //
        // `shutdown_timeout` gives tasks 500 ms to exit cleanly after the
        // shutdown token was triggered above.  The MCP task exits promptly
        // because it polls the `ShutdownToken`; gRPC tasks were already aborted.

        if let Some(network_rt) = app.state.network_rt.take() {
            tracing::info!("shutting down network runtime (gRPC, MCP tasks)...");
            network_rt
                .rt
                .shutdown_timeout(std::time::Duration::from_millis(500));
            tracing::info!("network runtime shutdown complete");
        }

        if app
            .state
            .terminal_surface_recovery_failed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err("windowed surface recovery failed terminally".into());
        }

        if app
            .state
            .benchmark_failed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err("windowed benchmark artifact write failed".into());
        }

        Ok(())
    }
}

#[cfg(test)]
mod wake_accounting_tests {
    use super::*;

    #[test]
    fn deferred_past_portal_deadline_waits_for_availability_not_immediate_retry() {
        let now = Instant::now();
        let past_deadline = PortalWakeDeadline {
            wall_us: 99,
            family: crate::portal_projection_driver::PortalDeadlineFamily::ImmediateWork,
        };

        assert_eq!(
            portal_deadline_after_drain(past_deadline, now, 100, PortalProjectionDrain::Deferred,),
            None,
            "a busy portal drain must not rearm its past deadline as a retry timer"
        );
        assert_eq!(
            portal_deadline_after_drain(
                past_deadline,
                now,
                100,
                PortalProjectionDrain::Completed {
                    scene_changed: false,
                },
            ),
            Some(Deadline::new(
                now,
                crate::idle_efficiency::RuntimeWakeupSource::SceneChange,
            )),
            "a successfully inspected past deadline still gets its one owning turn"
        );

        assert_eq!(
            portal_deadline_after_drain(
                PortalWakeDeadline {
                    wall_us: 101,
                    family: crate::portal_projection_driver::PortalDeadlineFamily::Cadence,
                },
                now,
                100,
                PortalProjectionDrain::Deferred,
            ),
            Some(Deadline::new(
                now + std::time::Duration::from_micros(1),
                crate::idle_efficiency::RuntimeWakeupSource::AnimationDeadline,
            )),
            "a genuinely future portal deadline remains a one-shot timed wake"
        );
    }

    #[test]
    fn terminal_lease_expiry_bridge_forwards_each_scene_result_once() {
        let sender = tze_hud_protocol::session_server::LeaseExpirySender::default();
        let mut receiver = sender.subscribe();
        let lease_id = tze_hud_scene::SceneId::new();
        let tile_id = tze_hud_scene::SceneId::new();

        publish_lease_expiries(
            Some(&sender),
            vec![tze_hud_scene::types::LeaseExpiry {
                lease_id,
                previous_state: tze_hud_scene::types::LeaseState::Active,
                terminal_state: tze_hud_scene::types::LeaseState::Expired,
                removed_tiles: vec![tile_id],
            }],
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let notice = runtime.block_on(receiver.recv()).unwrap();
        assert_eq!(notice.lease_id, lease_id);
        assert_eq!(
            notice.previous_state,
            tze_hud_scene::types::LeaseState::Active
        );
        assert_eq!(
            notice.terminal_state,
            tze_hud_scene::types::LeaseState::Expired
        );
        assert_eq!(notice.removed_tiles, vec![tile_id]);
        assert!(
            runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_millis(10), receiver.recv())
                    .await
                    .is_err()
            }),
            "one SceneGraph expiry result must produce exactly one bridge notice"
        );
    }

    #[tokio::test]
    async fn restarted_compositor_delivers_one_terminal_lease_pair_to_connected_owner() {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        use tokio_stream::StreamExt;
        use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
        use tze_hud_protocol::proto::session::hud_session_client::HudSessionClient;
        use tze_hud_protocol::proto::session::hud_session_server::HudSessionServer;
        use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
        use tze_hud_protocol::proto::session::{
            ClaimTile, ClientMessage, ReclaimReason, Reclaimed, RequestResult, ServerMessage,
            SessionInit,
        };
        use tze_hud_protocol::session_server::HudSessionImpl;
        use tze_hud_scene::types::{LeaseExpiry, LeaseState};

        fn now_wall_us() -> u64 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64
        }

        let service = HudSessionImpl::new(SceneGraph::new(800.0, 600.0), "test-key");
        let lease_expirations = service.lease_expirations.clone();
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            tonic::transport::Server::builder()
                .add_service(HudSessionServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });

        let endpoint = format!("http://[::1]:{}", addr.port());
        let mut client = None;
        for _ in 0..25 {
            match HudSessionClient::connect(endpoint.clone()).await {
                Ok(connected) => {
                    client = Some(connected);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        let mut client = client.expect("test session server must accept connections");

        let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(16);
        tx.send(ClientMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::SessionInit(SessionInit {
                agent_id: "restart-expiry-agent".to_string(),
                initial_subscriptions: Vec::new(),
                resume_token: Vec::new(),
                min_protocol_version: 1000,
                max_protocol_version: 1001,
                auth_credential: Some(tze_hud_protocol::auth::psk_credential(
                    "test-key".to_string(),
                )),
            })),
        })
        .await
        .unwrap();
        let mut stream = client
            .session(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        // SessionEstablished, SceneSnapshot, and initial DegradationNotice.
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .expect("session handshake must complete promptly")
                .expect("session stream must remain open")
                .expect("session handshake message must be valid");
        }

        tx.send(ClientMessage {
            sequence: 2,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ClientPayload::ClaimTile(ClaimTile {
                ttl_ms: 60_000,
                ..Default::default()
            })),
        })
        .await
        .unwrap();

        let mut granted_lease_id = None;
        while granted_lease_id.is_none() {
            let message = tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .expect("lease grant must arrive promptly")
                .expect("connected stream must remain open")
                .expect("lease grant message must be valid");
            match message.payload {
                Some(ServerPayload::RequestResult(RequestResult {
                    ok: granted,
                    lease_id,
                    ..
                })) => {
                    assert!(granted, "test lease must be granted");
                    granted_lease_id = uuid::Uuid::from_slice(&lease_id)
                        .ok()
                        .map(tze_hud_scene::SceneId::from_uuid);
                }
                other => panic!("expected lease grant traffic, got {other:?}"),
            }
        }
        let lease_id = granted_lease_id.expect("granted lease must carry a SceneId");

        // A mode switch tears down the first compositor generation. The runtime
        // must keep each sender so the replacement compositor receives a fresh
        // clone instead of silently dropping connected-session notifications.
        let mut runtime_state = WindowedRuntimeState::new_headless();
        let (frame_presented_tx, _frame_presented_rx) = tokio::sync::broadcast::channel(1);
        runtime_state.frame_presented_tx = Some(frame_presented_tx);
        runtime_state.degradation_notices = Some(Default::default());
        runtime_state.lease_expirations = Some(lease_expirations);
        let mut app = WinitApp {
            state: runtime_state,
        };

        let first_generation = app.state.compositor_runtime_senders();
        assert!(first_generation.0.is_some());
        assert!(first_generation.1.is_some());
        assert!(first_generation.2.is_some());
        drop(first_generation);

        app.state.pending_mode_switch = Some(WindowMode::Overlay);
        app.apply_pending_mode_switch();

        let restarted_generation = app.state.compositor_runtime_senders();
        assert!(app.state.frame_presented_tx.is_some());
        assert!(app.state.degradation_notices.is_some());
        assert!(app.state.lease_expirations.is_some());

        let expiry = LeaseExpiry {
            lease_id,
            previous_state: LeaseState::Active,
            terminal_state: LeaseState::Expired,
            removed_tiles: Vec::new(),
        };
        publish_lease_expiries(restarted_generation.2.as_ref(), vec![expiry.clone()]);
        publish_lease_expiries(restarted_generation.2.as_ref(), vec![expiry]);

        let mut terminal_response_seen = false;
        {
            let message: ServerMessage =
                tokio::time::timeout(Duration::from_secs(1), stream.next())
                    .await
                    .expect("terminal lease traffic must arrive promptly")
                    .expect("connected stream must remain open")
                    .expect("terminal lease message must be valid");
            match message.payload {
                Some(ServerPayload::Reclaimed(Reclaimed {
                    lease_id: response_lease_id,
                    why,
                    ..
                })) => {
                    assert_eq!(response_lease_id, lease_id.as_uuid().as_bytes().to_vec());
                    assert_eq!(why, ReclaimReason::Expired as i32);
                    assert!(
                        !terminal_response_seen,
                        "only one terminal response is allowed"
                    );
                    terminal_response_seen = true;
                }
                other => panic!("expected terminal lease traffic, got {other:?}"),
            }
        }
        assert!(terminal_response_seen);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), stream.next())
                .await
                .is_err(),
            "duplicate runtime notices must not emit a second terminal response"
        );

        drop(tx);
        server.abort();
    }

    #[test]
    fn passive_window_events_do_not_create_main_work_debt() {
        assert_eq!(
            main_work_source_for_window_event(&WindowEvent::RedrawRequested),
            None,
            "present polling must not schedule another compositor frame"
        );
        assert_eq!(
            main_work_source_for_window_event(&WindowEvent::CloseRequested),
            None,
            "shutdown owns its lifecycle without a post-settle render wake"
        );
        assert_eq!(
            main_work_source_for_window_event(&WindowEvent::Resized(
                winit::dpi::PhysicalSize::new(0, 0)
            )),
            None,
            "a zero-sized suspended surface has no renderable resize work"
        );
    }

    #[test]
    fn terminal_surface_recovery_triggers_gpu_lost_shutdown() {
        let shutdown = crate::threads::ShutdownToken::new();
        let mut shutdown_rx = shutdown.subscribe();
        let wake = WindowedWake::disconnected();
        let terminal_surface_recovery_failed = std::sync::atomic::AtomicBool::new(false);

        shutdown_after_terminal_surface_recovery(
            &shutdown,
            &wake,
            &terminal_surface_recovery_failed,
        );

        assert!(shutdown.is_triggered());
        assert!(
            terminal_surface_recovery_failed.load(std::sync::atomic::Ordering::Acquire),
            "the windowed run result must report the terminal recovery as non-zero"
        );
        assert_eq!(
            shutdown_rx.try_recv(),
            Ok(crate::threads::ShutdownReason::GpuDeviceLost),
            "terminal surface recovery must preserve the runtime's non-zero GPU-loss shutdown reason"
        );
        assert_eq!(
            wake.take_main_source(),
            crate::idle_efficiency::RuntimeWakeupSource::Shutdown,
            "the main event loop must be woken so it can observe and exit on the terminal transition"
        );
    }
}
