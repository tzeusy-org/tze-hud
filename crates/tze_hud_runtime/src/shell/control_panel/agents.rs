//! The owner's hands on agents: the roster, Clear, Unpair and allow-list
//! grants (CP-8). The Agents section reads and calls this; nothing here waits
//! on an agent (docs/invariants.md §3).

use std::sync::Arc;

use tokio::sync::Mutex;
use tze_hud_config::Grant;
use tze_hud_config::agents_file::{AgentChange, Edited};
use tze_hud_protocol::session::SharedState;
use tze_hud_protocol::session_server::LeaseExpirySender;
use tze_hud_scene::graph::{AgentHoldings, SceneGraph};
use tze_hud_scene::render_wake::RenderWakeNotifier;

use crate::agent_store::AgentStore;
use crate::portal_projection_driver::PORTAL_DRIVER_NAMESPACE;

/// How long Unpair waits for the agent's session tasks to end before it
/// clears regardless.
const SESSION_END_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// One agent as the panel lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentEntry {
    pub id: String,
    /// In `agents.toml` (a dev-PSK agent is not, and cannot be unpaired).
    pub paired: bool,
    pub allow: Vec<String>,
    /// Open gRPC sessions.
    pub sessions: usize,
    pub portals: usize,
    pub holdings: AgentHoldings,
}

impl AgentEntry {
    pub(crate) fn holds_nothing(&self) -> bool {
        self.portals == 0 && self.holdings.is_empty()
    }
}

/// What a Clear removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Cleared {
    pub portals: usize,
    pub holdings: AgentHoldings,
}

#[derive(Clone)]
pub(crate) struct AgentControl {
    pub store: Arc<AgentStore>,
    pub shared: Arc<Mutex<SharedState>>,
    pub scene: Arc<Mutex<SceneGraph>>,
    pub portals: Option<tze_hud_mcp::PortalHandle>,
    /// Forwards revoked leases to their sessions (`Reclaimed{OVERRIDE}`).
    pub lease_expirations: Option<LeaseExpirySender>,
    /// Reopened when the last paired agent is unpaired.
    pub pairing: Option<Arc<crate::pairing::Pairing>>,
    pub render_wake: RenderWakeNotifier,
    /// Wakes the winit loop, whose portal driver removes reclaimed surfaces.
    pub main_wake: RenderWakeNotifier,
}

impl AgentControl {
    /// Every paired agent, and every other namespace that is connected or
    /// holds something, sorted by id.
    pub(crate) async fn roster(&self) -> Vec<AgentEntry> {
        let store = Arc::clone(&self.store);
        let file = tokio::task::spawn_blocking(move || store.file())
            .await
            .unwrap_or_else(|error| {
                tracing::error!(%error, "agents.toml read failed; listing no paired agents");
                Default::default()
            });
        let portals = self
            .portals
            .as_ref()
            .and_then(|p| p.hub.lock().ok().map(|hub| hub.live_counts()))
            .unwrap_or_default();
        let rows: Vec<(String, usize, AgentHoldings)> = {
            let shared = self.shared.lock().await;
            let scene = self.scene.lock().await;
            let ids: std::collections::BTreeSet<String> = scene
                .agent_namespaces()
                .into_iter()
                .chain(file.agents.keys().cloned())
                .chain(portals.keys().cloned())
                .chain(shared.sessions.namespaces())
                .filter(|id| id != PORTAL_DRIVER_NAMESPACE)
                .collect();
            ids.into_iter()
                .map(|id| {
                    let (sessions, holdings) =
                        (shared.sessions.connected(&id), scene.agent_holdings(&id));
                    (id, sessions, holdings)
                })
                .collect()
        };
        rows.into_iter()
            .map(|(id, sessions, holdings)| {
                let record = file.agents.get(&id);
                AgentEntry {
                    paired: record.is_some(),
                    allow: record.map(|r| r.allow.clone()).unwrap_or_default(),
                    sessions,
                    portals: portals.get(&id).copied().unwrap_or(0),
                    holdings,
                    id,
                }
            })
            .collect()
    }

    /// Remove everything `agent` shows, now. Its next publish is served as
    /// usual (portals report `reattached: true`).
    pub(crate) async fn clear(&self, agent: &str) -> Cleared {
        let portals = match &self.portals {
            Some(p) => match p.hub.lock() {
                Ok(mut hub) => hub.reclaim_agent(agent),
                // A failed agent service never vetoes the owner.
                Err(_) => {
                    tracing::error!("portal hub poisoned; clearing the agent's other content");
                    0
                }
            },
            None => 0,
        };
        let reclaim = self.scene.lock().await.reclaim_agent(agent);
        crate::windowed::publish_lease_expiries(self.lease_expirations.as_ref(), reclaim.expiries);
        if portals > 0 {
            self.main_wake.notify();
        }
        self.render_wake.notify();
        Cleared {
            portals,
            holdings: reclaim.cleared,
        }
    }

    /// Forget `agent`: its PSK stops authenticating, its gRPC sessions end
    /// and its content clears. Unpairing the last agent reopens pairing.
    /// An agent already gone from the file (a hand edit) is still
    /// disconnected and cleared.
    pub(crate) async fn unpair(&self, agent: &str) -> Result<Cleared, String> {
        let edited = self.write(AgentChange::Unpair(agent.to_owned())).await?;
        let closing = self.shared.lock().await.sessions.close_namespace(agent);
        let closed = closing.len();
        // Clear once the sessions' tasks have dropped their handlers, so none
        // re-adds content after it. A task ends at its next poll; the bound
        // only guards against a runtime too busy to poll it.
        let ended = async {
            for closer in closing {
                closer.ended().await;
            }
        };
        if tokio::time::timeout(SESSION_END_WAIT, ended).await.is_err() {
            tracing::warn!(
                agent,
                "unpair: a session task had not ended; clearing anyway"
            );
        }
        let cleared = self.clear(agent).await;
        tracing::info!(
            agent,
            was_paired = edited.before.agents.contains_key(agent),
            sessions_closed = closed,
            portals = cleared.portals,
            zones = cleared.holdings.zones,
            widgets = cleared.holdings.widgets,
            tiles = cleared.holdings.tiles,
            "control panel: unpaired agent"
        );
        if edited.after.agents.is_empty()
            && let Some(pairing) = &self.pairing
        {
            pairing.open();
        }
        Ok(cleared)
    }

    /// Grant or withdraw one surface class, applied to the allow list as it
    /// stands in `agents.toml`. Applies to the agent's next MCP request; a
    /// connected gRPC session keeps the grants it connected with.
    pub(crate) async fn set_grant(
        &self,
        agent: &str,
        grant: Grant,
        on: bool,
    ) -> Result<(), String> {
        let edited = self
            .write(AgentChange::Grant {
                agent: agent.to_owned(),
                grant,
                on,
            })
            .await?;
        let allow = |f: &tze_hud_config::AgentsFile| {
            f.agents
                .get(agent)
                .map(|r| r.allow.clone())
                .unwrap_or_default()
        };
        let (old, new) = (allow(&edited.before), allow(&edited.after));
        if old != new {
            tracing::info!(agent, ?grant, on, ?old, ?new, "control panel: allow list");
        }
        Ok(())
    }

    async fn write(&self, change: AgentChange) -> Result<Edited, String> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.apply(&change))
            .await
            .map_err(|e| format!("agents.toml write failed: {e}"))?
    }
}
