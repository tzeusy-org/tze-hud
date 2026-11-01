//! Structured PSK authentication for new and resumed sessions.
use crate::proto::session::{AuthCredential, auth_credential::Credential};
use tze_hud_scene::config::{AgentDirectory, AgentIdentity, AuthRejection};

/// Build a pre-shared-key credential for a handshake.
pub fn psk_credential(key: impl Into<String>) -> AuthCredential {
    AuthCredential {
        credential: Some(Credential::PreSharedKey(
            crate::proto::session::PreSharedKeyCredential { key: key.into() },
        )),
    }
}

/// Resolve the paired PSK to its claimed agent and current permissions.
pub fn identify_session(
    agents: &AgentDirectory,
    auth_credential: Option<&AuthCredential>,
    claimed_agent_id: &str,
) -> Result<AgentIdentity, AuthRejection> {
    let key = match auth_credential.and_then(|c| c.credential.as_ref()) {
        Some(Credential::PreSharedKey(cred)) => cred.key.as_str(),
        None => "",
    };
    agents.resolve(key, claimed_agent_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directory() -> AgentDirectory {
        let mut agents = AgentDirectory::default();
        agents.insert(
            "agent",
            tze_hud_scene::config::hash_psk("secret"),
            Vec::new(),
        );
        agents
    }
    #[test]
    fn test_psk_credential_success() {
        assert!(identify_session(&directory(), Some(&psk_credential("secret")), "agent").is_ok());
    }
    #[test]
    fn test_psk_credential_failure() {
        assert!(identify_session(&directory(), Some(&psk_credential("wrong")), "agent").is_err());
    }
    #[test]
    fn test_empty_credential_fails() {
        for credential in [
            None,
            Some(AuthCredential { credential: None }),
            Some(psk_credential("")),
        ] {
            assert!(identify_session(&directory(), credential.as_ref(), "agent").is_err());
        }
    }
}
