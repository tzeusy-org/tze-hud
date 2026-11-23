//! Agent identity: which agent a PSK belongs to, and what it may do.
//!
//! Shared by the MCP and gRPC boundaries so both resolve identity the same
//! way. Only a SHA-256 digest of each agent's PSK is held; the plaintext PSK
//! never reaches the runtime's storage. The directory is loaded from
//! `agents.toml` by `tze_hud_config::agents_file` and shared live as
//! [`SharedAgents`], so swapping in a new directory takes effect on the next
//! request or handshake without a restart.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Agent id for a dev PSK presented with no claimed id (MCP).
pub const DEFAULT_MCP_AGENT_ID: &str = "mcp";

/// Internal permission granted by the `admin` allow entry. Held by exact
/// membership only: `*` does not imply it.
pub const OPERATOR_ADMIN: &str = "operator_admin";

/// SHA-256 digest of a PSK, as stored in `agents.toml`.
pub type PskDigest = [u8; 32];

/// Hash a PSK for storage and comparison.
pub fn hash_psk(psk: &str) -> PskDigest {
    Sha256::digest(psk.as_bytes()).into()
}

/// A resolved caller: its agent id (which is also its namespace) and its
/// internal permissions (expanded from the `allow` list).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentity {
    pub agent_id: String,
    pub permissions: Vec<String>,
    /// Runtime pairing identity; ordinary permission edits preserve it.
    pub authorization: PairingAuthorization,
}

impl AgentIdentity {
    /// True when the identity holds `permission` (or is unrestricted).
    pub fn allows(&self, permission: &str) -> bool {
        self.permissions
            .iter()
            .any(|p| p == "*" || p == permission || wildcard_match(p, permission))
    }
}

impl AgentIdentity {
    /// True only when the `admin` allow entry was granted explicitly; `*` and
    /// wildcards never grant it.
    pub fn is_operator_admin(&self) -> bool {
        self.permissions.iter().any(|p| p == OPERATOR_ADMIN)
    }
}

/// `publish_zone:*` matches `publish_zone:<anything>`.
fn wildcard_match(granted: &str, wanted: &str) -> bool {
    granted
        .strip_suffix('*')
        .is_some_and(|prefix| prefix.ends_with(':') && wanted.starts_with(prefix))
}

/// Why a credential was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthRejection {
    pub code: &'static str,
    pub message: String,
    pub hint: String,
}

/// Opaque lifetime of one pairing, shared by all its resident sessions.
///
/// The guard linearizes raster admission with revocation. Grants belong to this
/// pairing rather than a transport, and disappear when it is revoked. Callers
/// must finish all awaits before entering the synchronous commit closure.
#[derive(Clone)]
pub struct PairingAuthorization(PairingScope);

#[derive(Clone)]
enum PairingScope {
    Paired(Arc<Mutex<PairingState>>),
    Dev {
        state: Arc<Mutex<DevPairings>>,
        namespace: String,
        generation: u64,
    },
}

struct PairingState {
    live: bool,
    raster_grants: HashSet<[u8; 32]>,
}

impl Default for PairingAuthorization {
    fn default() -> Self {
        Self(PairingScope::Paired(Arc::new(Mutex::new(PairingState {
            live: true,
            raster_grants: HashSet::new(),
        }))))
    }
}

/// Resolving an arbitrary dev namespace allocates no durable entry. Entries
/// below arise only from actual grants or namespace pairing transitions.
#[derive(Default)]
struct DevPairings {
    generations: HashMap<String, u64>,
    grants: HashMap<String, HashSet<[u8; 32]>>,
}

impl DevPairings {
    fn revoke(&mut self, namespace: &str) {
        let generation = self.generations.entry(namespace.to_owned()).or_default();
        *generation = generation
            .checked_add(1)
            .expect("pairing generation exhausted");
        self.grants.remove(namespace);
    }
}

impl std::fmt::Debug for PairingAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingAuthorization(..)")
    }
}

impl PartialEq for PairingAuthorization {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (PairingScope::Paired(a), PairingScope::Paired(b)) => Arc::ptr_eq(a, b),
            (
                PairingScope::Dev {
                    state: a,
                    namespace: an,
                    generation: ag,
                },
                PairingScope::Dev {
                    state: b,
                    namespace: bn,
                    generation: bg,
                },
            ) => Arc::ptr_eq(a, b) && an == bn && ag == bg,
            _ => false,
        }
    }
}
impl Eq for PairingAuthorization {}

impl PairingAuthorization {
    /// Execute one non-suspending grant check or commit unless revoked.
    pub fn with_live_grants<T>(
        &self,
        commit: impl FnOnce(&mut HashSet<[u8; 32]>) -> T,
    ) -> Option<T> {
        match &self.0 {
            PairingScope::Paired(state) => {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                state.live.then(|| commit(&mut state.raster_grants))
            }
            PairingScope::Dev {
                state,
                namespace,
                generation,
            } => {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                if state
                    .generations
                    .get(namespace)
                    .copied()
                    .unwrap_or_default()
                    != *generation
                {
                    return None;
                }
                let mut grants = state.grants.remove(namespace).unwrap_or_default();
                let result = commit(&mut grants);
                if !grants.is_empty() {
                    state.grants.insert(namespace.clone(), grants);
                }
                Some(result)
            }
        }
    }

    fn revoke(&self) {
        match &self.0 {
            PairingScope::Paired(state) => {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                state.live = false;
                state.raster_grants.clear();
            }
            PairingScope::Dev {
                state,
                namespace,
                generation,
            } => {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                if state
                    .generations
                    .get(namespace)
                    .copied()
                    .unwrap_or_default()
                    == *generation
                {
                    state.revoke(namespace);
                }
            }
        }
    }
}

#[derive(Clone)]
struct PairedAgent {
    psk_sha256: PskDigest,
    permissions: Vec<String>,
    authorization: PairingAuthorization,
}

/// The runtime's trusted agents, keyed by agent id.
#[derive(Clone, Default)]
pub struct AgentDirectory {
    agents: HashMap<String, PairedAgent>,
    /// Plaintext dev/test PSK that may claim any agent id. Only
    /// [`AgentDirectory::unrestricted`] sets it.
    dev_psk: Option<String>,
    dev_authorizations: Arc<Mutex<DevPairings>>,
    /// Captured by this immutable directory, so stale snapshots stay revoked.
    dev_generations: Arc<HashMap<String, u64>>,
}

/// The live agent directory shared by MCP and gRPC. `store` a new directory
/// to add or remove agents without a restart.
pub type SharedAgents = Arc<ArcSwap<AgentDirectory>>;

impl std::fmt::Debug for AgentDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut ids: Vec<&String> = self.agents.keys().collect();
        ids.sort();
        f.debug_struct("AgentDirectory")
            .field("agents", &ids)
            .field("dev_psk", &self.dev_psk.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl AgentDirectory {
    /// Dev/test directory: `dev_psk` identifies any claimed agent id (or
    /// [`DEFAULT_MCP_AGENT_ID`] with none). A claimed id with an entry gets
    /// that entry's permissions; any other id is unrestricted. An empty
    /// `dev_psk` adds nothing.
    pub fn unrestricted(dev_psk: impl Into<String>) -> Self {
        let dev_psk = dev_psk.into();
        Self {
            agents: HashMap::new(),
            dev_psk: (!dev_psk.is_empty()).then_some(dev_psk),
            dev_authorizations: Default::default(),
            dev_generations: Default::default(),
        }
    }

    /// Add or replace an agent by the SHA-256 digest of its PSK.
    pub fn insert(
        &mut self,
        agent_id: impl Into<String>,
        psk_sha256: PskDigest,
        permissions: Vec<String>,
    ) {
        self.agents.insert(
            agent_id.into(),
            PairedAgent {
                psk_sha256,
                permissions,
                authorization: PairingAuthorization::default(),
            },
        );
    }

    /// This directory's dev PSK with `paired`'s agents: what the runtime
    /// authenticates against after `agents.toml` changes.
    pub fn with_agents_of(&self, paired: AgentDirectory) -> Self {
        self.with_agent_change(paired, None)
    }

    /// Commit a validated directory transition before publishing its ArcSwap.
    /// Explicit Pair/Unpair also invalidate a dev-only claimed namespace.
    pub fn with_agent_change(&self, mut paired: AgentDirectory, replaced: Option<&str>) -> Self {
        for (id, old) in &self.agents {
            match paired.agents.get_mut(id) {
                Some(new) if replaced != Some(id.as_str()) && old.psk_sha256 == new.psk_sha256 => {
                    new.authorization = old.authorization.clone();
                }
                _ => old.authorization.revoke(),
            }
        }
        let mut changed: HashSet<&str> = self
            .agents
            .keys()
            .chain(paired.agents.keys())
            .map(String::as_str)
            .filter(|id| match (self.agents.get(*id), paired.agents.get(*id)) {
                (Some(old), Some(new)) => old.psk_sha256 != new.psk_sha256,
                _ => true,
            })
            .collect();
        changed.extend(replaced);
        let mut dev = self
            .dev_authorizations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for id in changed {
            dev.revoke(id);
        }
        let generations = Arc::new(dev.generations.clone());
        drop(dev);
        Self {
            agents: paired.agents,
            dev_psk: self.dev_psk.clone(),
            dev_authorizations: Arc::clone(&self.dev_authorizations),
            dev_generations: generations,
        }
    }

    /// Each paired agent's id and whether it holds `admin`, sorted by id.
    /// Never includes credentials.
    pub fn summaries(&self) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = self
            .agents
            .iter()
            .map(|(id, a)| {
                (
                    id.clone(),
                    a.permissions.iter().any(|p| p == OPERATOR_ADMIN),
                )
            })
            .collect();
        out.sort();
        out
    }

    /// True when `agent_id` has an entry.
    pub fn contains(&self, agent_id: &str) -> bool {
        self.agents.contains_key(agent_id)
    }

    /// True when no credential can authenticate.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty() && self.dev_psk.is_none()
    }

    /// Wrap the directory for live sharing.
    pub fn shared(self) -> SharedAgents {
        Arc::new(ArcSwap::from_pointee(self))
    }

    /// Resolve a credential to an agent.
    ///
    /// The credential is hashed and compared in constant time against every
    /// stored digest; a match identifies that agent, and `claimed_id`, if
    /// non-empty, must name it. The dev PSK identifies `claimed_id` instead.
    pub fn resolve(
        &self,
        credential: &str,
        claimed_id: &str,
    ) -> Result<AgentIdentity, AuthRejection> {
        if credential.is_empty() {
            return Err(reject("no PSK presented"));
        }
        if let Some(dev_psk) = &self.dev_psk
            && bool::from(credential.as_bytes().ct_eq(dev_psk.as_bytes()))
        {
            return Ok(self.dev_identity(claimed_id));
        }
        let digest = hash_psk(credential);
        let mut owner = None;
        for (id, agent) in &self.agents {
            if bool::from(digest.ct_eq(&agent.psk_sha256)) {
                owner = Some((id, agent));
            }
        }
        let Some((agent_id, agent)) = owner else {
            return Err(reject("PSK does not match any paired agent"));
        };
        if !claimed_id.is_empty() && claimed_id != agent_id {
            return Err(AuthRejection {
                code: "AUTH_FAILED",
                message: format!("PSK belongs to agent {agent_id:?}, not {claimed_id:?}"),
                hint: "present this agent's own PSK".into(),
            });
        }
        Ok(AgentIdentity {
            agent_id: agent_id.clone(),
            permissions: agent.permissions.clone(),
            authorization: agent.authorization.clone(),
        })
    }

    fn dev_identity(&self, claimed_id: &str) -> AgentIdentity {
        let agent_id = if claimed_id.is_empty() {
            DEFAULT_MCP_AGENT_ID
        } else {
            claimed_id
        };
        let permissions = self
            .agents
            .get(agent_id)
            .map_or_else(|| vec!["*".to_string()], |a| a.permissions.clone());
        let authorization = self.agents.get(agent_id).map_or_else(
            || {
                PairingAuthorization(PairingScope::Dev {
                    state: Arc::clone(&self.dev_authorizations),
                    namespace: agent_id.to_owned(),
                    generation: self
                        .dev_generations
                        .get(agent_id)
                        .copied()
                        .unwrap_or_default(),
                })
            },
            |agent| agent.authorization.clone(),
        );
        AgentIdentity {
            agent_id: agent_id.to_string(),
            permissions,
            authorization,
        }
    }
}

fn reject(message: &str) -> AuthRejection {
    AuthRejection {
        code: "AUTH_FAILED",
        message: message.to_string(),
        hint: "send the agent's paired PSK as the bearer / auth_credential".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> AgentDirectory {
        let mut dir = AgentDirectory::default();
        dir.insert("claude", hash_psk("claude-key"), vec!["*".to_string()]);
        dir.insert(
            "bot",
            hash_psk("bot-key"),
            vec!["publish_zone:*".to_string()],
        );
        dir
    }

    #[test]
    fn resolve_accepts_the_psk_whose_hash_is_stored_and_rejects_others() {
        let id = dir().resolve("bot-key", "").unwrap();
        assert_eq!(id.agent_id, "bot");
        assert!(id.allows("publish_zone:subtitle"));
        assert!(!id.allows("create_tiles"));
        assert_eq!(
            dir().resolve("claude-key", "claude").unwrap().agent_id,
            "claude"
        );

        for (psk, claim) in [("nope", ""), ("", ""), ("bot-key", "claude")] {
            assert_eq!(dir().resolve(psk, claim).unwrap_err().code, "AUTH_FAILED");
        }
        // The stored value is the digest, never the PSK.
        assert!(!format!("{:?}", dir()).contains("bot-key"));
    }

    #[test]
    fn dev_psk_claims_any_id_with_entry_permissions_or_unrestricted() {
        let mut dev = AgentDirectory::unrestricted("dev");
        dev.insert("bot", hash_psk("bot-key"), vec!["publish_zone:*".into()]);
        let open = dev.resolve("dev", "").unwrap();
        assert_eq!(open.agent_id, DEFAULT_MCP_AGENT_ID);
        assert!(open.allows("resident_mcp"));
        assert!(!dev.resolve("dev", "bot").unwrap().allows("create_tiles"));
        assert!(
            dev.resolve("dev", "stranger")
                .unwrap()
                .allows("create_tiles")
        );
        assert!(AgentDirectory::unrestricted("").is_empty());
        assert!(!format!("{dev:?}").contains("\"dev\""));
        for i in 0..1_000 {
            let identity = dev.resolve("dev", &format!("no-grant-{i}")).unwrap();
            assert!(
                identity
                    .authorization
                    .with_live_grants(|g| g.is_empty())
                    .unwrap()
            );
        }
        {
            let state = dev.dev_authorizations.lock().unwrap();
            assert!(state.generations.is_empty() && state.grants.is_empty());
        }
        let a = dev.resolve("dev", "dev-a").unwrap().authorization;
        let b = dev.resolve("dev", "dev-b").unwrap().authorization;
        a.with_live_grants(|g| g.insert([1; 32])).unwrap();
        b.with_live_grants(|g| g.insert([2; 32])).unwrap();
        // Explicit Unpair of an unconfigured dev namespace revokes only its
        // current work; a fresh dev handshake remains unrestricted.
        let changed = dev.with_agent_change(dev.clone(), Some("dev-a"));
        assert!(a.with_live_grants(|_| ()).is_none());
        assert!(
            dev.resolve("dev", "dev-a")
                .unwrap()
                .authorization
                .with_live_grants(|_| ())
                .is_none(),
            "old directory snapshots must not acquire the new generation"
        );
        let fresh = changed.resolve("dev", "dev-a").unwrap().authorization;
        assert_ne!(a, fresh);
        assert!(fresh.with_live_grants(|g| g.is_empty()).unwrap());
        assert!(b.with_live_grants(|g| g.contains(&[2; 32])).unwrap());
        let edited = changed.with_agents_of(changed.clone());
        assert_eq!(fresh, edited.resolve("dev", "dev-a").unwrap().authorization);
    }
}
