//! Agent session management — authentication, capabilities, session state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::{Mutex, mpsc};
use tonic::Status;
use tze_hud_resource::{ResourceStore, RuntimeWidgetStore};
use tze_hud_scene::SceneId;
use tze_hud_scene::element_store::ElementStore;
use tze_hud_scene::graph::SceneGraph;

use crate::proto::session::ServerMessage;
use crate::token::TokenStore;

/// Runtime input-capture command for the local input processor owned by the
/// compositor/window thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputCaptureCommand {
    Request {
        tile_id: SceneId,
        node_id: SceneId,
        device_id: u32,
        release_on_up: bool,
    },
    Release {
        device_id: u32,
    },
}

/// Shared state between the gRPC server and the compositor.
///
/// # Scene coherence
///
/// `scene` is an `Arc<Mutex<SceneGraph>>` shared across both the gRPC session
/// server and the MCP server.  Callers that already hold the `SharedState`
/// mutex should acquire the inner scene lock by calling
/// `st.scene.lock().await`.  The compositor thread pre-clones the `Arc` at
/// startup and locks it independently (never holding the outer `SharedState`
/// lock while waiting for the inner lock) to avoid nested-lock priority
/// inversion.
pub struct SharedState {
    pub scene: Arc<Mutex<SceneGraph>>,
    pub sessions: SessionRegistry,
    /// Resident scene-resource upload store (RFC 0011 on HudSession stream).
    pub resource_store: ResourceStore,
    /// Durable runtime widget asset store (v1 scoped durability exception).
    pub runtime_widget_store: Option<RuntimeWidgetStore>,
    /// Persistent element identity store (zone/widget/tile Scene IDs).
    pub element_store: ElementStore,
    /// On-disk path for `element_store.toml`. When `None`, persistence is disabled.
    pub element_store_path: Option<PathBuf>,
    /// Whether the runtime is currently in safe mode (RFC 0005 §3.7).
    ///
    /// This is the **single source of truth** for the runtime-global safe-mode
    /// flag.  When `true`, all active sessions reject MutationBatch with
    /// SAFE_MODE_ACTIVE, and the winit event thread captures input locally.
    ///
    /// It is an `AtomicBool` so it can be read lock-free on the winit event
    /// thread (`dispatch_key_down_event` / `dispatch_key_up_event` /
    /// `dispatch_character_event` hot paths) without acquiring the
    /// `SharedState` mutex.  Writers (exclusively
    /// `shell::safe_mode::enter_safe_mode` and `exit_safe_mode`) store with
    /// `Ordering::Release`; readers load with `Ordering::Acquire`.  The
    /// Release-Acquire pair guarantees that any stores preceding the flag
    /// write are visible to the event thread once it observes the raised flag,
    /// even though the reader does not hold the `SharedState` mutex.
    ///
    /// Mutation-intake readers (e.g. `handle_mutation_batch`) hold the
    /// `SharedState` mutex and likewise load with `Ordering::Acquire`.
    pub safe_mode_atomic: Arc<AtomicBool>,
    /// Lock-free mirror of `scene.active_tab` for the winit event thread.
    ///
    /// The composer keystroke-echo path (`dispatch_key_down_event` →
    /// composer intercept) must apply local feedback within the 4 ms
    /// input-to-local-ack budget ("Local feedback first" doctrine) and MUST
    /// NOT be blocked by gRPC scene-mutation batches that hold the scene
    /// `Mutex`.  Reading `scene.active_tab` for the tab-id guard previously
    /// required `try_lock`ing the scene mutex; under sustained portal
    /// streaming that try_lock kept failing, so keystrokes were deferred and
    /// the echo froze (hud-dwcr7).
    ///
    /// This mirror is a tiny dedicated `std::sync::Mutex<Option<SceneId>>`
    /// that is **never** held across an `.await` and **never** nested with the
    /// scene mutex — locking it is a single `Option<SceneId>` copy, so it can
    /// never reproduce the scene-mutex starvation.  Writers refresh it via
    /// [`SharedState::refresh_active_tab_mirror`] whenever they hold the scene
    /// and may have changed `active_tab` (gRPC mutation apply, event-loop tab
    /// switch).  A one-frame lag is acceptable: the scene remains the source of
    /// truth and the mirror reconverges on the next refresh.
    pub active_tab_mirror: Arc<std::sync::Mutex<Option<SceneId>>>,
    /// In-memory resume token store (RFC 0005 §6.1).
    /// Cleared on process restart; never persisted.
    pub token_store: TokenStore,
    /// Runtime-wide freeze state (system-shell/spec.md §Freeze Scene).
    ///
    /// The shell is the sole writer of this field. When `freeze_active` is
    /// `true`, mutation batches are queued (not rejected) until unfreeze.
    ///
    /// Per the invariant: `safe_mode_atomic == true` implies
    /// `freeze_active = false`. Safe mode entry cancels freeze
    /// and discards all per-session freeze queues.
    pub freeze_active: bool,
    /// Optional bridge for session-plane pointer capture requests. Windowed
    /// runtime installs the receiving end; the gRPC session plane no longer
    /// sends commands (the capture requests were removed in T5 S6).
    pub input_capture_tx: Option<mpsc::UnboundedSender<InputCaptureCommand>>,
    /// Main-loop-only wake paired with `input_capture_tx`. Successful command
    /// enqueue uses this to wake the sole windowed consumer without creating a
    /// speculative compositor generation.
    pub input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    /// Claimed-tile sizes and spacing from `[design_tokens]` `tile.*`, used
    /// to resolve `ClaimTile` placement hints.
    pub tile_placement: tze_hud_scene::placement::TilePlacementTokens,
}

impl SharedState {
    /// Refresh the lock-free `active_tab_mirror` from the authoritative
    /// `scene.active_tab`.  Call this from any path that holds the scene and
    /// may have changed the active tab (gRPC mutation apply, event-loop tab
    /// switch).  Best-effort: a poisoned mirror lock is recovered in place
    /// since the stored value is a plain `Copy` `Option<SceneId>` with no
    /// invariant to corrupt.
    pub fn refresh_active_tab_mirror(&self, scene: &SceneGraph) {
        let value = scene.active_tab;
        let mut guard = self
            .active_tab_mirror
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = value;
    }

    /// Inspect the active-tab mirror in keyboard-dispatch fixtures without
    /// touching the scene mutex.
    #[cfg(any(test, feature = "dev-mode"))]
    pub fn active_tab_mirror_value(&self) -> Option<SceneId> {
        self.active_tab_mirror
            .lock()
            .map(|g| *g)
            .unwrap_or_else(|poisoned| *poisoned.into_inner())
    }
}

/// Bounded per-session event channel capacity (events).
pub const SESSION_EVENT_CHANNEL_CAPACITY: usize = 256;

/// A connected agent session.
#[derive(Debug)]
pub struct AgentSession {
    pub session_id: String,
    pub namespace: String,
    pub agent_name: String,
    pub capabilities: Vec<String>,
    pub lease_ids: Vec<SceneId>,
    pub event_subscribed: bool,
    /// Sender half of the per-session ServerMessage channel.
    ///
    /// Used by the safe mode controller to deliver `SessionSuspended` and
    /// `SessionResumed` messages outside the normal event subscription path.
    /// Registered by the session handler when the stream is established.
    /// These messages are transactional (never dropped) — per RFC 0005 §3.1.
    pub server_message_tx: Option<mpsc::Sender<Result<ServerMessage, Status>>>,
}

impl Clone for AgentSession {
    fn clone(&self) -> Self {
        // server_message_tx is not cloned — the channel is owned by the session record.
        Self {
            session_id: self.session_id.clone(),
            namespace: self.namespace.clone(),
            agent_name: self.agent_name.clone(),
            capabilities: self.capabilities.clone(),
            lease_ids: self.lease_ids.clone(),
            event_subscribed: self.event_subscribed,
            server_message_tx: None,
        }
    }
}

/// Session registry for connected agents. Sessions are registered after the
/// handshake has resolved the agent's identity.
#[derive(Default)]
pub struct SessionRegistry {
    sessions: HashMap<String, AgentSession>,
    /// Observers of the full normal cleanup boundary, never a cleanup trigger.
    #[cfg(test)]
    cleanup_observers: HashMap<SceneId, Vec<tokio::sync::oneshot::Sender<()>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a session for an authenticated agent.
    pub fn register(&mut self, agent_name: &str, capabilities: &[String]) -> AgentSession {
        let session_id = uuid::Uuid::now_v7().to_string();
        let session = AgentSession {
            session_id: session_id.clone(),
            namespace: agent_name.to_string(),
            agent_name: agent_name.to_string(),
            capabilities: capabilities.to_vec(),
            lease_ids: Vec::new(),
            event_subscribed: false,
            server_message_tx: None,
        };
        self.sessions.insert(session_id, session.clone());
        session
    }

    pub fn remove_session(&mut self, session_id: &str) -> Option<AgentSession> {
        self.sessions.remove(session_id)
    }

    /// Register before dropping the transport, keyed by the scene session ID
    /// carried by SessionEstablished (distinct from the internal registry ID).
    /// Simultaneous connections in one namespace stay distinct; oneshots retain
    /// completion even when cleanup finishes before the observer awaits.
    #[cfg(test)]
    pub(crate) fn observe_cleanup(
        &mut self,
        session_id: &SceneId,
    ) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cleanup_observers
            .entry(*session_id)
            .or_default()
            .push(tx);
        rx
    }

    /// Called only after normal lease/token/upload/enforcer cleanup completes.
    /// RegistryGuard's removal-only fallback must not signal this boundary.
    #[cfg(test)]
    pub(crate) fn finish_cleanup(&mut self, session_id: &SceneId) {
        if let Some(observers) = self.cleanup_observers.remove(session_id) {
            for observer in observers {
                let _ = observer.send(());
            }
        }
    }

    /// Inspect the connected-session count in lifecycle fixtures.
    #[cfg(any(test, feature = "dev-mode"))]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Find a namespace owner in session-routing fixtures.
    #[cfg(any(test, feature = "dev-mode"))]
    pub fn session_for_namespace(&self, namespace: &str) -> Option<&AgentSession> {
        self.sessions.values().find(|s| s.namespace == namespace)
    }

    /// Broadcast a `ServerMessage` to all connected sessions via their direct server channels.
    ///
    /// Used by the safe mode controller to deliver `SessionSuspended` and `SessionResumed`
    /// to all active session streams.  These messages are transactional (RFC 0005 §3.1) and
    /// must not be dropped; if the channel is full the send will fail and the drop is logged
    /// as a warning (the session's backpressure signal path handles overflow recovery).
    ///
    /// Returns the count of sessions that received the message.
    pub fn broadcast_server_message(&self, msg: ServerMessage) -> usize {
        let mut sent = 0;
        for session in self.sessions.values() {
            if let Some(tx) = &session.server_message_tx {
                if let Err(e) = tx.try_send(Ok(msg.clone())) {
                    tracing::warn!(
                        session_id = %session.session_id,
                        "Failed to deliver transactional ServerMessage (channel full or closed); message dropped: {}",
                        e
                    );
                } else {
                    sent += 1;
                }
            }
        }
        sent
    }

    /// Register the `ServerMessage` sender for an existing session.
    ///
    /// Called by the session handler when a new session stream is established.
    /// Allows the safe mode controller to deliver out-of-band control messages
    /// (`SessionSuspended`, `SessionResumed`) to all active sessions.
    ///
    /// Returns `true` if the sender was registered successfully, `false` if the
    /// `session_id` is not found in the registry.
    pub fn register_server_message_tx(
        &mut self,
        session_id: &str,
        tx: mpsc::Sender<Result<ServerMessage, Status>>,
    ) -> bool {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.server_message_tx = Some(tx);
            true
        } else {
            false
        }
    }
}
