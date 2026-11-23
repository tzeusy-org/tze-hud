//! Service struct for the bidirectional streaming session server.
//!
//! This module contains the `HudSessionImpl` struct definition, its
//! constructor, and the non-session-loop runtime helper methods.
//!
//! The `async fn session` dispatch loop (the `HudSession` trait impl) remains in
//! `session_server/mod.rs` as a separate `impl HudSession for HudSessionImpl`
//! block, which is valid Rust (split impl across files in the same module).

use super::SharedMutationBudgetEnforcer;
use crate::session::SharedState;
use std::sync::Arc;
use tokio::sync::Mutex;
#[cfg(any(test, feature = "dev-mode"))]
use tze_hud_resource::{ResourceStore, ResourceStoreConfig};
use tze_hud_scene::config::SharedAgents;
#[cfg(any(test, feature = "dev-mode"))]
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::ResourceBudget;

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
    pub(super) publication_defaults: tze_hud_scene::PublicationDefaults,
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
}

/// Everything the session service takes from the runtime.
///
/// [`SessionDeps::new`] fills the optional parts with defaults; set the
/// fields that differ before passing it to [`HudSessionImpl::from_deps`].
pub struct SessionDeps {
    pub publication_defaults: tze_hud_scene::PublicationDefaults,
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
            publication_defaults: Default::default(),
            budget_enforcer: None,
            degradation_notices: super::DegradationNoticeSender::default(),
            render_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
        }
    }
}

impl HudSessionImpl {
    /// Build the publication, upload and lifecycle service from runtime owners.
    pub fn from_deps(deps: SessionDeps) -> Self {
        Self {
            state: deps.state,
            publication_defaults: deps.publication_defaults,
            render_wake: deps.render_wake,
            agents: deps.agents,
            resource_budget: deps.resource_budget,
            budget_enforcer: deps.budget_enforcer,
            degradation_notices: deps.degradation_notices,
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
            element_store_file: None,
            safe_mode_atomic: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            active_tab_mirror: Arc::new(std::sync::Mutex::new(None)),
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
}
