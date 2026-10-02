//! Agent identity: which agent a PSK belongs to, and what it may do.
//!
//! Shared by the MCP and gRPC boundaries so both resolve identity the same
//! way. Only a SHA-256 digest of each agent's PSK is held; the plaintext PSK
//! never reaches the runtime's storage. The directory is loaded from
//! `agents.toml` by `tze_hud_config::agents_file` and shared live as
//! [`SharedAgents`], so swapping in a new directory takes effect on the next
//! request or handshake without a restart.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Agent id for a dev PSK presented with no claimed id (MCP).
pub const DEFAULT_MCP_AGENT_ID: &str = "mcp";

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
}

impl AgentIdentity {
    /// True when the identity holds `permission` (or is unrestricted).
    pub fn allows(&self, permission: &str) -> bool {
        self.permissions
            .iter()
            .any(|p| p == "*" || p == permission || wildcard_match(p, permission))
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

#[derive(Clone)]
struct PairedAgent {
    psk_sha256: PskDigest,
    permissions: Vec<String>,
}

/// The runtime's trusted agents, keyed by agent id.
#[derive(Clone, Default)]
pub struct AgentDirectory {
    agents: HashMap<String, PairedAgent>,
    /// Plaintext dev/test PSK that may claim any agent id. Only
    /// [`AgentDirectory::unrestricted`] sets it.
    dev_psk: Option<String>,
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
            },
        );
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
        })
    }
}

impl AgentDirectory {
    /// Resolve a loopback local-socket caller: identified like the dev PSK,
    /// and rejected when there is none (production).
    pub fn resolve_local(&self, claimed_id: &str) -> Result<AgentIdentity, AuthRejection> {
        match self.dev_psk {
            Some(_) => Ok(self.dev_identity(claimed_id)),
            None => Err(reject("local-socket credentials identify no paired agent")),
        }
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
        AgentIdentity {
            agent_id: agent_id.to_string(),
            permissions,
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
    }
}
