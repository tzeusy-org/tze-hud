//! Service struct for the bidirectional streaming session server.
//!
//! This module contains the `HudSessionImpl` struct definition, its constructors,
//! and the non-session-loop runtime helper methods. Moved from
//! `session_server/mod.rs` as Step SS-5 of the module split
//! (docs/design/session-server-renderer-module-split-plan.md §3.4).
//!
//! The `async fn session` dispatch loop (the `HudSession` trait impl) remains in
//! `session_server/mod.rs` as a separate `impl HudSession for HudSessionImpl`
//! block, which is valid Rust (split impl across files in the same module).

use super::SharedMutationBudgetEnforcer;
use super::stream_session::CapabilityRevocationEvent;
use crate::convert;
use crate::proto::session::DegradationNotice;
use crate::session::SharedState;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
#[cfg(any(test, feature = "dev-mode"))]
use tze_hud_resource::{ResourceStore, ResourceStoreConfig};
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
/// `agent_capabilities` drives per-agent capability gating at handshake time
/// (configuration/spec.md §Requirement: Agent Registration, lines 136-147).
/// Agents whose `agent_id` matches a key in this map receive only the listed
/// capabilities; unlisted agents are treated as guests (no capabilities).
pub struct HudSessionImpl {
    pub state: Arc<Mutex<SharedState>>,
    /// Runtime-owned callback that wakes the windowed event/compositor loops
    /// after render-relevant session work is accepted or enqueued.
    pub(super) render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    pub(super) psk: String,
    /// Per-agent capability grants from `[agents.registered]` config.
    ///
    /// Keyed by agent name (the `agent_id` sent in `SessionInit`).
    /// Used to build `CapabilityPolicy` at handshake: registered agents get
    /// their listed capabilities; unregistered agents get guest (empty) policy.
    ///
    /// For dev/test scenarios where no config is loaded, pass an empty map
    /// and set `fallback_unrestricted = true` to restore the legacy behaviour.
    pub(super) agent_capabilities: Arc<HashMap<String, Vec<String>>>,
    /// Frozen per-agent mutation/lease budgets derived at runtime startup.
    pub(super) agent_resource_budgets: Arc<HashMap<String, ResourceBudget>>,
    /// Budget applied to agents without an explicit registered override.
    pub(super) fallback_resource_budget: ResourceBudget,
    /// Runtime-owned mutation-intake enforcement bridge.
    pub(super) budget_enforcer: Option<SharedMutationBudgetEnforcer>,
    /// When true and an agent is not found in `agent_capabilities`, grant
    /// unrestricted capabilities (backwards-compatible dev mode).
    ///
    /// Production deployments MUST set this to `false`.
    pub(super) fallback_unrestricted: bool,
    /// Bounded never-drop sender for transactional degradation notices.
    pub degradation_notices: super::DegradationNoticeSender,
    /// Runtime-to-session durable bridge for terminal lease transitions.
    ///
    /// The compositor publishes each `SceneGraph::expire_leases()` result here
    /// after releasing the scene lock. The handler that owns the lease emits
    /// the wire `LeaseResponse` and `LeaseStateChange` transactionally.
    pub lease_expirations: super::LeaseExpirySender,
    /// Broadcast sender for live capability revocation commands (RFC 0001 §3.3, GAP-G3-4).
    ///
    /// When the runtime calls `revoke_capability_on_lease`, it broadcasts a
    /// `CapabilityRevocationEvent` here. Each active session handler subscribes
    /// and processes revocations for leases it owns, applying the scene-graph
    /// mutation and delivering the `CapabilityNotice` + `LeaseStateChange` responses.
    pub capability_revocation_tx: tokio::sync::broadcast::Sender<CapabilityRevocationEvent>,

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

impl HudSessionImpl {
    /// Create a new session service with the given scene graph and PSK.
    ///
    /// Uses an empty capability registry with `fallback_unrestricted = true`
    /// for backwards compatibility. Prefer `new_with_config` for production.
    #[cfg(any(test, feature = "dev-mode"))]
    pub fn new(scene: SceneGraph, psk: &str) -> Self {
        let degradation_notices = super::DegradationNoticeSender::default();
        let lease_expirations = super::LeaseExpirySender::default();
        let (capability_revocation_tx, _) =
            tokio::sync::broadcast::channel(super::BROADCAST_CHANNEL_CAPACITY);
        let input_event_tx = super::InputEventSender::new(super::BROADCAST_CHANNEL_CAPACITY);
        let (element_repositioned_tx, _) =
            tokio::sync::broadcast::channel(super::BROADCAST_CHANNEL_CAPACITY);
        let (frame_presented_tx, _) =
            tokio::sync::broadcast::channel(super::BROADCAST_CHANNEL_CAPACITY);
        Self {
            state: Arc::new(Mutex::new(SharedState {
                scene: Arc::new(Mutex::new(scene)),
                sessions: crate::session::SessionRegistry::new(psk),
                resource_store: ResourceStore::new(ResourceStoreConfig::default()),
                widget_asset_store: crate::session::WidgetAssetStore::default(),
                runtime_widget_store: None,
                element_store: tze_hud_scene::element_store::ElementStore::default(),
                element_store_path: None,
                safe_mode_atomic: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                active_tab_mirror: Arc::new(std::sync::Mutex::new(None)),
                token_store: crate::token::TokenStore::new(),
                freeze_active: false,
                degradation_level: crate::session::RuntimeDegradationLevel::Normal,
                input_capture_tx: None,
                input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
                resolved_portal_tokens: std::collections::HashMap::new(),
            })),
            render_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            psk: psk.to_string(),
            agent_capabilities: Arc::new(HashMap::new()),
            agent_resource_budgets: Arc::new(HashMap::new()),
            fallback_resource_budget: ResourceBudget::default(),
            budget_enforcer: None,
            fallback_unrestricted: true,
            degradation_notices,
            lease_expirations,
            capability_revocation_tx,
            input_event_tx,
            element_repositioned_tx,
            frame_presented_tx,
        }
    }

    /// Create from existing shared state with a config-driven capability registry.
    ///
    /// `agent_capabilities` is populated from `ResolvedConfig::agent_capabilities`
    /// (i.e. the `[agents.registered]` TOML section).
    ///
    /// `fallback_unrestricted` controls what happens when an agent is NOT found in
    /// the registry:
    /// - `false` (production): unlisted agents receive guest policy (no capabilities).
    /// - `true` (dev/test): unlisted agents receive unrestricted policy.
    pub fn from_shared_state_with_config(
        state: Arc<Mutex<SharedState>>,
        psk: &str,
        agent_capabilities: HashMap<String, Vec<String>>,
        fallback_unrestricted: bool,
    ) -> Self {
        Self::from_shared_state_with_config_and_degradation_notices(
            state,
            psk,
            agent_capabilities,
            fallback_unrestricted,
            super::DegradationNoticeSender::default(),
        )
    }

    pub fn from_shared_state_with_config_and_degradation_notices(
        state: Arc<Mutex<SharedState>>,
        psk: &str,
        agent_capabilities: HashMap<String, Vec<String>>,
        fallback_unrestricted: bool,
        degradation_notices: super::DegradationNoticeSender,
    ) -> Self {
        Self::from_shared_state_with_runtime_envelope_and_degradation_notices(
            state,
            psk,
            agent_capabilities,
            HashMap::new(),
            ResourceBudget::default(),
            fallback_unrestricted,
            None,
            degradation_notices,
        )
    }
    /// Create from shared state with the immutable production runtime envelope.
    #[allow(clippy::too_many_arguments)]
    pub fn from_shared_state_with_runtime_envelope(
        state: Arc<Mutex<SharedState>>,
        psk: &str,
        agent_capabilities: HashMap<String, Vec<String>>,
        agent_resource_budgets: HashMap<String, ResourceBudget>,
        fallback_resource_budget: ResourceBudget,
        fallback_unrestricted: bool,
        budget_enforcer: Option<SharedMutationBudgetEnforcer>,
    ) -> Self {
        Self::from_shared_state_with_runtime_envelope_and_degradation_notices(
            state,
            psk,
            agent_capabilities,
            agent_resource_budgets,
            fallback_resource_budget,
            fallback_unrestricted,
            budget_enforcer,
            super::DegradationNoticeSender::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_shared_state_with_runtime_envelope_and_degradation_notices(
        state: Arc<Mutex<SharedState>>,
        psk: &str,
        agent_capabilities: HashMap<String, Vec<String>>,
        agent_resource_budgets: HashMap<String, ResourceBudget>,
        fallback_resource_budget: ResourceBudget,
        fallback_unrestricted: bool,
        budget_enforcer: Option<SharedMutationBudgetEnforcer>,
        degradation_notices: super::DegradationNoticeSender,
    ) -> Self {
        let (capability_revocation_tx, _) =
            tokio::sync::broadcast::channel(super::BROADCAST_CHANNEL_CAPACITY);
        let lease_expirations = super::LeaseExpirySender::default();
        let input_event_tx = super::InputEventSender::new(super::BROADCAST_CHANNEL_CAPACITY);
        let (element_repositioned_tx, _) =
            tokio::sync::broadcast::channel(super::BROADCAST_CHANNEL_CAPACITY);
        let (frame_presented_tx, _) =
            tokio::sync::broadcast::channel(super::BROADCAST_CHANNEL_CAPACITY);
        Self {
            state,
            render_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            psk: psk.to_string(),
            agent_capabilities: Arc::new(agent_capabilities),
            agent_resource_budgets: Arc::new(agent_resource_budgets),
            fallback_resource_budget,
            budget_enforcer,
            fallback_unrestricted,
            degradation_notices,
            lease_expirations,
            capability_revocation_tx,
            input_event_tx,
            element_repositioned_tx,
            frame_presented_tx,
        }
    }

    /// Bind the production render-work wake seam without exposing a platform
    /// event-loop type to the protocol crate.
    pub fn with_render_wake_notifier(
        mut self,
        notifier: tze_hud_scene::render_wake::RenderWakeNotifier,
    ) -> Self {
        self.render_wake = notifier;
        self
    }

    /// Broadcast a `DegradationNotice` to all currently-active sessions.
    ///
    /// Updates `SharedState::degradation_level` so that newly-joining sessions
    /// can observe the current level. Then sends the notice on the broadcast
    /// channel so every active session handler delivers it transactionally.
    ///
    /// Returns the number of active sessions that received the notice (0 if
    /// no sessions are connected).
    pub async fn broadcast_degradation(
        &self,
        level: crate::session::RuntimeDegradationLevel,
        reason: &str,
        affected_capabilities: Vec<String>,
    ) -> usize {
        // Update shared state.
        {
            let mut st = self.state.lock().await;
            st.degradation_level = level;
        }

        let notice = DegradationNotice {
            level: level.to_proto_i32(),
            reason: reason.to_string(),
            affected_capabilities,
            timestamp_wall_us: super::now_wall_us(),
        };

        // Broadcast returns an error only when there are no active subscribers
        // (no sessions connected). That is not an error condition.
        self.degradation_notices.publish(notice).await
    }

    /// Revoke a named capability from an active lease at runtime (RFC 0001 §3.3, GAP-G3-4).
    ///
    /// This is the end-to-end API for live capability revocation. It:
    /// 1. Broadcasts a [`CapabilityRevocationEvent`] to all active session handlers.
    /// 2. The session handler that owns `lease_id` receives the event, calls
    ///    [`tze_hud_scene::graph::SceneGraph::revoke_capability`] to narrow the live scope,
    ///    then delivers `CapabilityNotice(revoked=[capability_name])` and a `LeaseStateChange`
    ///    audit event to the affected agent.
    ///
    /// After revocation, any attempt to use `capability_name` under `lease_id` will be
    /// rejected by the existing capability-check path in the mutation pipeline.
    ///
    /// # Arguments
    ///
    /// * `lease_id`        — The lease whose capability scope is being narrowed.
    /// * `capability_name` — Canonical name of the capability to remove
    ///   (e.g. `"create_tiles"`, `"publish_zone:subtitle"`).
    ///
    /// # Returns
    ///
    /// The number of session handlers that received the revocation event (0 if the
    /// lease is not owned by any currently-connected session).
    pub fn revoke_capability_on_lease(
        &self,
        lease_id: tze_hud_scene::SceneId,
        capability_name: impl Into<String>,
    ) -> usize {
        let event = CapabilityRevocationEvent {
            lease_id,
            capability_name: capability_name.into(),
        };
        self.capability_revocation_tx
            .send(event)
            .unwrap_or_default()
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
