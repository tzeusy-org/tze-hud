//! Agent identity: which agent a PSK belongs to, and what it may do.
//!
//! Shared by the MCP and gRPC boundaries so both resolve identity the same
//! way. Pure and I/O-free: PSK values are resolved from the environment by the
//! runtime before this is built.

use std::collections::HashMap;

use subtle::ConstantTimeEq;

/// Namespace used for MCP callers presenting the runtime PSK when no
/// configured agent owns that PSK.
pub const DEFAULT_MCP_AGENT_ID: &str = "mcp";

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

/// The runtime's trusted agents.
#[derive(Clone, Debug, Default)]
pub struct AgentDirectory {
    /// The runtime PSK (`--psk` / `TZE_HUD_PSK`). Empty means "none".
    pub runtime_psk: String,
    /// Agent id → its own PSK (from `psk_env`).
    pub agent_psks: HashMap<String, String>,
    /// Agent id → internal permissions expanded from `allow`.
    pub permissions: HashMap<String, Vec<String>>,
    /// Permissions for an agent with no `[agents.<id>]` table. `["*"]` when no
    /// agents are configured (dev/test); empty in production.
    pub fallback_permissions: Vec<String>,
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

impl AgentDirectory {
    /// A directory with only a runtime PSK and unrestricted agents (dev/test).
    pub fn unrestricted(runtime_psk: impl Into<String>) -> Self {
        Self {
            runtime_psk: runtime_psk.into(),
            fallback_permissions: vec!["*".to_string()],
            ..Default::default()
        }
    }

    fn permissions_for(&self, agent_id: &str) -> Vec<String> {
        self.permissions
            .get(agent_id)
            .cloned()
            .unwrap_or_else(|| self.fallback_permissions.clone())
    }

    /// Resolve a credential to an agent.
    ///
    /// - A credential equal to an agent's own PSK identifies that agent;
    ///   `claimed_id`, if non-empty, must match it.
    /// - The runtime PSK identifies `claimed_id` (gRPC), or, with no claimed
    ///   id (MCP), the configured agent whose own PSK is the runtime PSK, else
    ///   [`DEFAULT_MCP_AGENT_ID`]. An agent with its own distinct PSK cannot be
    ///   claimed with the runtime PSK.
    pub fn resolve(
        &self,
        credential: &str,
        claimed_id: &str,
    ) -> Result<AgentIdentity, AuthRejection> {
        if credential.is_empty() {
            return Err(reject("no PSK presented"));
        }
        let is_runtime = !self.runtime_psk.is_empty() && ct_eq(credential, &self.runtime_psk);
        if !is_runtime {
            let owner = self
                .agent_psks
                .iter()
                .find(|(_, psk)| ct_eq(credential, psk))
                .map(|(id, _)| id.clone());
            let Some(agent_id) = owner else {
                return Err(reject("PSK does not match any agent"));
            };
            if !claimed_id.is_empty() && claimed_id != agent_id {
                return Err(AuthRejection {
                    code: "AUTH_FAILED",
                    message: format!("PSK belongs to agent {agent_id:?}, not {claimed_id:?}"),
                    hint: "present the PSK from this agent's own psk_env".into(),
                });
            }
            let permissions = self.permissions_for(&agent_id);
            return Ok(AgentIdentity {
                agent_id,
                permissions,
            });
        }
        let agent_id = if claimed_id.is_empty() {
            self.agent_psks
                .iter()
                .find(|(_, psk)| ct_eq(psk, &self.runtime_psk))
                .map(|(id, _)| id.clone())
                .unwrap_or_else(|| DEFAULT_MCP_AGENT_ID.to_string())
        } else {
            claimed_id.to_string()
        };
        if let Some(own) = self.agent_psks.get(&agent_id)
            && !ct_eq(own, &self.runtime_psk)
        {
            return Err(AuthRejection {
                code: "AUTH_FAILED",
                message: format!("agent {agent_id:?} has its own PSK"),
                hint: "present the PSK from this agent's psk_env, not the runtime PSK".into(),
            });
        }
        let permissions = self.permissions_for(&agent_id);
        Ok(AgentIdentity {
            agent_id,
            permissions,
        })
    }
}

fn reject(message: &str) -> AuthRejection {
    AuthRejection {
        code: "AUTH_FAILED",
        message: message.to_string(),
        hint: "send the runtime PSK or the agent's own PSK as the bearer / auth_credential"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> AgentDirectory {
        AgentDirectory {
            runtime_psk: "runtime".into(),
            agent_psks: [
                ("claude".to_string(), "runtime".to_string()),
                ("bot".to_string(), "bot-key".to_string()),
            ]
            .into(),
            permissions: [
                ("claude".to_string(), vec!["*".to_string()]),
                ("bot".to_string(), vec!["publish_zone:*".to_string()]),
            ]
            .into(),
            fallback_permissions: vec![],
        }
    }

    #[test]
    fn own_psk_identifies_agent() {
        let id = dir().resolve("bot-key", "").unwrap();
        assert_eq!(id.agent_id, "bot");
        assert!(id.allows("publish_zone:subtitle"));
        assert!(!id.allows("create_tiles"));
    }

    #[test]
    fn runtime_psk_without_claim_resolves_to_agent_owning_it() {
        assert_eq!(dir().resolve("runtime", "").unwrap().agent_id, "claude");
    }

    #[test]
    fn runtime_psk_cannot_claim_agent_with_own_psk() {
        assert_eq!(
            dir().resolve("runtime", "bot").unwrap_err().code,
            "AUTH_FAILED"
        );
        assert!(dir().resolve("bot-key", "claude").is_err());
    }

    #[test]
    fn unknown_agent_gets_fallback_and_wrong_psk_fails() {
        let id = dir().resolve("runtime", "stranger").unwrap();
        assert!(id.permissions.is_empty());
        assert!(dir().resolve("nope", "").is_err());
        let open = AgentDirectory::unrestricted("k").resolve("k", "").unwrap();
        assert_eq!(open.agent_id, DEFAULT_MCP_AGENT_ID);
        assert!(open.allows("resident_mcp"));
    }
}
