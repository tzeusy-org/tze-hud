//! Handshake authentication and identity tests.
//!
//! Credential evaluation, PSK → agent identity resolution, and version
//! negotiation.

use tze_hud_protocol::auth::identify_session;
use tze_hud_protocol::proto::session::auth_credential::Credential;
use tze_hud_protocol::proto::session::{AuthCredential, PreSharedKeyCredential};
use tze_hud_scene::config::{AgentDirectory, hash_psk};

// ─── Identity ────────────────────────────────────────────────────────────────

fn directory() -> AgentDirectory {
    let mut dir = AgentDirectory::default();
    dir.insert(
        "claude",
        hash_psk("claude-psk"),
        vec!["publish_zone:subtitle".to_string()],
    );
    dir
}

fn psk(key: &str) -> AuthCredential {
    AuthCredential {
        credential: Some(Credential::PreSharedKey(PreSharedKeyCredential {
            key: key.to_string(),
        })),
    }
}

/// A paired agent's PSK identifies it and yields its allow-derived permissions.
#[test]
fn own_psk_identifies_agent_with_allow_permissions() {
    let id = identify_session(&directory(), Some(&psk("claude-psk")), "claude").unwrap();
    assert_eq!(id.agent_id, "claude");
    assert!(id.allows("publish_zone:subtitle"));
    assert!(!id.allows("create_tiles"));
}

/// An agent's PSK cannot be used to claim a different agent id.
#[test]
fn own_psk_cannot_claim_other_agent() {
    let err = identify_session(&directory(), Some(&psk("claude-psk")), "other").unwrap_err();
    assert_eq!(err.code, "AUTH_FAILED");
    assert!(!err.hint.is_empty());
}

/// An unpaired PSK identifies no one, whatever id it claims.
#[test]
fn unpaired_psk_is_rejected() {
    for claim in ["claude", "stranger", ""] {
        assert!(identify_session(&directory(), Some(&psk("runtime")), claim).is_err());
    }
}
