//! Service struct for the bidirectional streaming session server.
//!
//! This module contains the `HudSessionImpl` struct definition, its
//! constructor, and the non-session-loop runtime helper methods.
//!
//! The `async fn session` dispatch loop (the `HudSession` trait impl) remains in
//! `session_server/mod.rs` as a separate `impl HudSession for HudSessionImpl`
//! block, which is valid Rust (split impl across files in the same module).

use super::SharedMutationBudgetEnforcer;
use crate::convert;
use crate::session::SharedState;
use std::sync::Arc;
use tokio::sync::Mutex;
#[cfg(any(test, feature = "dev-mode"))]
use tze_hud_resource::{ResourceStore, ResourceStoreConfig};
use tze_hud_scene::config::SharedAgents;
#[cfg(any(test, feature = "dev-mode"))]
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{GeometryPolicy, ResourceBudget, SceneId};

// ─── Service implementation ─────────────────────────────────────────────────

/// The bidirectional streaming session service implementation.
///
/// Holds shared state (scene graph + session registry) and implements the
/// `HudSession` trait generated from `session.proto`.
///
/// `degradation_notices` is a bounded per-session transactional hub. It applies
/// producer backpressure instead of allowing lag/drop semantics.
///
/// `agents` resolves the handshake credential to an agent identity and its
/// `allow`-derived permissions (see [`AgentDirectory`]).
pub struct HudSessionImpl {
    pub state: Arc<Mutex<SharedState>>,
    /// Runtime-owned callback that wakes the windowed event/compositor loops
    /// after render-relevant session work is accepted or enqueued.
    pub(super) render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    /// Credential → agent identity and permissions, shared live with MCP;
    /// loaded once per handshake.
    pub(super) agents: SharedAgents,
    /// Mutation/lease budget applied to every session.
    pub(super) resource_budget: ResourceBudget,
    /// Runtime-owned mutation-intake enforcement bridge.
    pub(super) budget_enforcer: Option<SharedMutationBudgetEnforcer>,
    /// Bounded never-drop sender for transactional degradation notices.
    pub degradation_notices: super::DegradationNoticeSender,
    /// Runtime-to-session durable bridge for terminal lease transitions.
    ///
    /// The compositor publishes each `SceneGraph::expire_leases()` result here
    /// after releasing the scene lock. The handler that owns the lease emits
    /// the wire `LeaseResponse` transactionally.
    pub lease_expirations: super::LeaseExpirySender,

    /// Traffic-class-aware sender for runtime-injected input event batches (hud-i6yd.6).
    ///
    /// Carries `(namespace, EventBatch)` tuples. Each session handler subscribes
    /// and delivers the batch only if `namespace` matches its own namespace AND the
    /// agent has at least one of `INPUT_EVENTS` / `FOCUS_EVENTS` active. The batch
    /// is filtered through `subscriptions::filter_event_batch` before delivery.
    ///
    /// Transactional variants use a durable per-session lane; ephemeral and
    /// state-stream variants use bounded broadcast delivery.
    pub input_event_tx: super::InputEventSender,

    /// Broadcast sender for `ElementRepositionedEvent` notifications (hud-bs2q.6).
    ///
    /// Emitted after drag completion (geometry_override persisted) and after
    /// reset-to-default (geometry_override cleared). Each session handler subscribes
    /// and delivers the event only when the agent is subscribed to `SCENE_TOPOLOGY`
    /// and the session is `Active`. Agents cannot reject — no response mechanism.
    ///
    /// Subscription category: SCENE_TOPOLOGY (requires `read_scene_topology`).
    /// Message class: Transactional (never coalesced or dropped).
    pub element_repositioned_tx:
        tokio::sync::broadcast::Sender<crate::proto::ElementRepositionedEvent>,

    /// Broadcast sender for `FramePresented` acknowledgments (hud-91uu6).
    ///
    /// Carries batch-correlated present timing: each event pairs the
    /// MutationBatch.batch_ids composited into a presented frame with that
    /// frame's number and present wall-clock. The render loop (headless or
    /// windowed) drains the scene's present-ack queue at frame present and
    /// sends here via [`Self::broadcast_frame_presented`]. Each session handler
    /// subscribes and delivers only when the agent is subscribed to
    /// `TELEMETRY_FRAMES` (which requires the `read_telemetry` capability).
    ///
    /// Subscription category: TELEMETRY_FRAMES (requires `read_telemetry`).
    /// Message class: State-stream (coalesced/droppable under backpressure).
    pub frame_presented_tx: tokio::sync::broadcast::Sender<crate::proto::FramePresented>,
}

/// Everything the session service takes from the runtime.
///
/// [`SessionDeps::new`] fills the optional parts with defaults; set the
/// fields that differ before passing it to [`HudSessionImpl::from_deps`].
pub struct SessionDeps {
    pub state: Arc<Mutex<SharedState>>,
    /// Credential → agent identity and permissions, shared live with MCP.
    pub agents: SharedAgents,
    /// Mutation/lease budget applied to every session.
    pub resource_budget: ResourceBudget,
    /// Runtime-owned mutation-intake enforcement bridge.
    pub budget_enforcer: Option<SharedMutationBudgetEnforcer>,
    /// Transactional degradation-notice hub, shared with the runtime.
    pub degradation_notices: super::DegradationNoticeSender,
    /// Wakes the windowed event/compositor loops after render-relevant work.
    pub render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
}

impl SessionDeps {
    pub fn new(state: Arc<Mutex<SharedState>>, agents: SharedAgents) -> Self {
        Self {
            state,
            agents,
            resource_budget: ResourceBudget::default(),
            budget_enforcer: None,
            degradation_notices: super::DegradationNoticeSender::default(),
            render_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
        }
    }
}

/// Per-session receivers for the runtime → session feeds.
pub(super) struct RuntimeFeeds {
    /// Durable lane for terminal lease transitions.
    pub lease_expiry: super::LeaseExpiryReceiver,
    /// Input events; the per-namespace durable lane is subscribed only after
    /// authentication establishes the namespace.
    pub input_events: super::InputEventSender,
    /// Delivered when the agent holds the SCENE_TOPOLOGY subscription.
    pub element_repositioned:
        tokio::sync::broadcast::Receiver<crate::proto::ElementRepositionedEvent>,
    /// Delivered when the agent holds the TELEMETRY_FRAMES subscription.
    pub frame_presented: tokio::sync::broadcast::Receiver<crate::proto::FramePresented>,
}

impl HudSessionImpl {
    pub(super) fn subscribe_feeds(&self) -> RuntimeFeeds {
        RuntimeFeeds {
            lease_expiry: self.lease_expirations.subscribe(),
            input_events: self.input_event_tx.clone(),
            element_repositioned: self.element_repositioned_tx.subscribe(),
            frame_presented: self.frame_presented_tx.subscribe(),
        }
    }

    /// Build the service. The per-feature runtime → session channels are
    /// created here, in one place.
    pub fn from_deps(deps: SessionDeps) -> Self {
        let (element_repositioned_tx, _) =
            tokio::sync::broadcast::channel(super::BROADCAST_CHANNEL_CAPACITY);
        let (frame_presented_tx, _) =
            tokio::sync::broadcast::channel(super::BROADCAST_CHANNEL_CAPACITY);
        Self {
            state: deps.state,
            render_wake: deps.render_wake,
            agents: deps.agents,
            resource_budget: deps.resource_budget,
            budget_enforcer: deps.budget_enforcer,
            degradation_notices: deps.degradation_notices,
            lease_expirations: super::LeaseExpirySender::default(),
            input_event_tx: super::InputEventSender::new(super::BROADCAST_CHANNEL_CAPACITY),
            element_repositioned_tx,
            frame_presented_tx,
        }
    }

    /// Dev/test service over `scene` whose dev PSK claims any agent id.
    #[cfg(any(test, feature = "dev-mode"))]
    pub fn new(scene: SceneGraph, psk: &str) -> Self {
        let state = Arc::new(Mutex::new(SharedState {
            scene: Arc::new(Mutex::new(scene)),
            sessions: crate::session::SessionRegistry::new(),
            resource_store: ResourceStore::new(ResourceStoreConfig::default()),
            runtime_widget_store: None,
            element_store: tze_hud_scene::element_store::ElementStore::default(),
            element_store_path: None,
            safe_mode_atomic: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            active_tab_mirror: Arc::new(std::sync::Mutex::new(None)),
            token_store: crate::token::TokenStore::new(),
            freeze_active: false,
            input_capture_tx: None,
            input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            tile_placement: Default::default(),
        }));
        Self::from_deps(SessionDeps::new(
            state,
            tze_hud_scene::config::AgentDirectory::unrestricted(psk).shared(),
        ))
    }

    /// Pair agents with these permissions (expanded `allow` lists); the dev
    /// PSK claims them by id. Test-only: production loads `agents.toml`.
    #[cfg(test)]
    pub(crate) fn with_agent_permissions(
        self,
        permissions: std::collections::HashMap<String, Vec<String>>,
    ) -> Self {
        let mut agents = tze_hud_scene::config::AgentDirectory::clone(&self.agents.load());
        for (id, perms) in permissions {
            let digest = tze_hud_scene::config::hash_psk(&format!("{id}-own-psk"));
            agents.insert(id, digest, perms);
        }
        self.agents.store(Arc::new(agents));
        self
    }

    /// Inject an `EventBatch` into the gRPC stream of the session owning `namespace`.
    ///
    /// Used by the runtime to push ClickEvent / CommandInputEvent batches produced by
    /// the compositor input pipeline (Stage 2) to the owning agent (hud-i6yd.6).
    ///
    /// The batch is fanned out to all session handler tasks; each task delivers it only
    /// if its namespace matches AND the event passes subscription filtering
    /// (`INPUT_EVENTS` / `FOCUS_EVENTS` gates).
    ///
    /// Returns the number of session handlers that received the batch (0 if no
    /// sessions are currently connected, regardless of namespace match).
    ///
    /// # Subscription gate
    ///
    /// ClickEvent and CommandInputEvent are `INPUT_EVENTS` variants. The session handler
    /// will silently drop the batch if the agent is not subscribed to `INPUT_EVENTS`.
    /// Callers that need a guaranteed delivery path should ensure the agent subscribes
    /// to `INPUT_EVENTS` / `access_input_events` at handshake time.
    pub fn inject_input_event(
        &self,
        namespace: impl Into<String>,
        batch: crate::proto::EventBatch,
    ) -> usize {
        self.input_event_tx.send((namespace.into(), batch))
    }

    /// Broadcast an `ElementRepositionedEvent` to all active sessions subscribed
    /// to `SCENE_TOPOLOGY` (hud-bs2q.6).
    ///
    /// Called after:
    /// - Drag completion: `geometry_override` has been persisted.
    /// - Reset-to-default: `geometry_override` has been cleared.
    ///
    /// Each session handler delivers the event only when:
    /// 1. The session is `SessionState::Active`.
    /// 2. The agent is subscribed to `SCENE_TOPOLOGY`.
    ///
    /// Returns the number of active session handlers that received the broadcast
    /// (0 if no sessions are connected).
    pub fn broadcast_element_repositioned(
        &self,
        event: crate::proto::ElementRepositionedEvent,
    ) -> usize {
        self.element_repositioned_tx.send(event).unwrap_or_default()
    }

    /// Broadcast a `FramePresented` acknowledgment to all active sessions
    /// subscribed to `TELEMETRY_FRAMES` (hud-91uu6).
    ///
    /// Called by the render loop once per presented frame that carried one or
    /// more accepted mutation batches, pairing those `batch_ids` with the
    /// presented frame number and present wall-clock. Each session handler
    /// delivers the event only when the agent is subscribed to
    /// `TELEMETRY_FRAMES` (which requires the `read_telemetry` capability).
    ///
    /// Returns the number of active session handlers that received the broadcast
    /// (0 if no sessions are connected).
    pub fn broadcast_frame_presented(&self, event: crate::proto::FramePresented) -> usize {
        self.frame_presented_tx.send(event).unwrap_or_default()
    }

    /// Reset an element's user geometry override to the fallback position and
    /// broadcast an `ElementRepositionedEvent` to subscribed agents (hud-bs2q.6).
    ///
    /// This is the programmatic path for "reset-to-default". The visual entry
    /// point (right-click context menu / tap button on the drag handle) calls
    /// this from the compositor/input pipeline.
    ///
    /// # Behaviour
    ///
    /// 1. Clears `geometry_override` from the element store entry.
    /// 2. If no override was set, returns `false` (no-op).
    /// 3. Re-resolves the effective geometry from the fallback chain:
    ///    agent bounds → config override → default policy.
    /// 4. Persists the element store to disk.
    /// 5. Broadcasts `ElementRepositionedEvent {
    ///        element_id,
    ///        new_geometry  = fallback geometry,
    ///        previous_geometry = cleared override,
    ///    }` to sessions subscribed to `SCENE_TOPOLOGY`.
    ///
    /// Returns `true` if an override was cleared and the event was emitted.
    pub async fn reset_element_geometry(&self, element_id: SceneId) -> bool {
        let (previous_override, fallback_geometry, persist_request) = {
            let mut st = self.state.lock().await;
            // Attempt to clear the override.
            let previous = st.element_store.reset_geometry_override(element_id);
            if previous.is_none() {
                // No override present — no-op.
                return false;
            }
            // Resolve fallback geometry (agent bounds → config → default policy).
            let scene = st.scene.lock().await;
            let fallback = st
                .element_store
                .entries
                .get(&element_id)
                .map(|entry| {
                    tze_hud_scene::element_store::fallback_geometry_for_element(
                        element_id, entry, &scene,
                    )
                })
                .unwrap_or(tze_hud_scene::ZERO_GEOMETRY_POLICY);
            drop(scene);
            let persist_req =
                st.element_store_path
                    .clone()
                    .map(|path| super::ElementStorePersistRequest {
                        store: st.element_store.clone(),
                        path,
                    });
            (
                previous.expect(
                    "invariant: the `previous.is_none()` check above returns early, so \
                     `previous` is guaranteed `Some` at this point",
                ),
                fallback,
                persist_req,
            )
        };

        // Persist store outside the lock.
        super::persist_element_store(persist_request).await;

        // Build and broadcast ElementRepositionedEvent.
        let event = crate::proto::ElementRepositionedEvent {
            element_id: super::scene_id_to_bytes(element_id),
            new_geometry: Some(convert::geometry_policy_to_proto(&fallback_geometry)),
            previous_geometry: Some(convert::geometry_policy_to_proto(&previous_override)),
        };
        self.broadcast_element_repositioned(event);
        true
    }

    /// Build and broadcast an `ElementRepositionedEvent` for a completed drag
    /// (hud-bs2q.6).
    ///
    /// Called by the compositor after `persist_drag_geometry` has already written
    /// the new `geometry_override` to the element store.
    ///
    /// `new_geometry` is the newly persisted policy.
    /// `previous_geometry` is the geometry that was in effect before the drag
    /// (the prior override or `None` if there was no override).
    pub fn emit_drag_repositioned_event(
        &self,
        element_id: SceneId,
        new_geometry: &GeometryPolicy,
        previous_geometry: Option<&GeometryPolicy>,
    ) {
        let event = crate::proto::ElementRepositionedEvent {
            element_id: super::scene_id_to_bytes(element_id),
            new_geometry: Some(convert::geometry_policy_to_proto(new_geometry)),
            previous_geometry: previous_geometry.map(convert::geometry_policy_to_proto),
        };
        self.broadcast_element_repositioned(event);
    }
}
