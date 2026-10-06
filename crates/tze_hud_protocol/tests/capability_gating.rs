//! Handshake authentication and identity tests.
//!
//! Credential evaluation, PSK → agent identity resolution, and version
//! negotiation.

use std::net::IpAddr;

use tze_hud_protocol::auth::{
    RUNTIME_MAX_VERSION, RUNTIME_MIN_VERSION, identify_session, negotiate_version,
};
use tze_hud_protocol::proto::session::auth_credential::Credential;
use tze_hud_protocol::proto::session::{
    AuthCredential, LocalSocketCredential, PreSharedKeyCredential,
};
use tze_hud_scene::config::{AgentDirectory, hash_psk};

fn loopback() -> Option<IpAddr> {
    Some("127.0.0.1".parse().unwrap())
}

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
    let id = identify_session(&directory(), Some(&psk("claude-psk")), "", "claude", None).unwrap();
    assert_eq!(id.agent_id, "claude");
    assert!(id.allows("publish_zone:subtitle"));
    assert!(!id.allows("create_tiles"));
}

/// An agent's PSK cannot be used to claim a different agent id.
#[test]
fn own_psk_cannot_claim_other_agent() {
    let err =
        identify_session(&directory(), Some(&psk("claude-psk")), "", "other", None).unwrap_err();
    assert_eq!(err.code, "AUTH_FAILED");
    assert!(!err.hint.is_empty());
}

/// An unpaired PSK identifies no one, whatever id it claims.
#[test]
fn unpaired_psk_is_rejected() {
    for claim in ["claude", "stranger", ""] {
        assert!(identify_session(&directory(), Some(&psk("runtime")), "", claim, None).is_err());
    }
}

/// Loopback LocalSocketCredential is treated like the dev PSK: it claims an
/// id under a dev directory and identifies no one in production.
#[test]
fn local_socket_from_loopback_resolves_only_under_dev_psk() {
    let cred = AuthCredential {
        credential: Some(Credential::LocalSocket(LocalSocketCredential::default())),
    };
    let dev = AgentDirectory::unrestricted("dev");
    let id = identify_session(&dev, Some(&cred), "", "stranger", loopback()).unwrap();
    assert_eq!(id.agent_id, "stranger");
    assert!(identify_session(&directory(), Some(&cred), "", "stranger", loopback()).is_err());
}

// ─── Protocol version negotiation ────────────────────────────────────────────

/// WHEN agent supports v1.0 THEN negotiation succeeds at v1.0.
#[test]
fn version_negotiation_succeeds_at_v1_0() {
    let version = negotiate_version(1000, 1000);
    assert!(version.is_ok());
    assert_eq!(version.unwrap(), 1000);
}

/// WHEN agent supports v1.0-v1.1 THEN negotiation picks highest (v1.1).
#[test]
fn version_negotiation_picks_highest_mutual_version() {
    let version = negotiate_version(1000, 1001);
    assert!(version.is_ok());
    assert_eq!(version.unwrap(), RUNTIME_MAX_VERSION);
}

/// WHEN agent supports v0.9 only THEN negotiation fails.
#[test]
fn version_negotiation_fails_when_no_intersection() {
    // Agent only supports pre-v1.0
    let version = negotiate_version(900, 999);
    assert!(
        version.is_err(),
        "version negotiation must fail when agent range does not intersect runtime range"
    );
}

/// WHEN agent sends min=0, max=0 (unset) THEN treated as v1.0.
#[test]
fn version_negotiation_treats_zero_as_v1_0() {
    let version = negotiate_version(0, 0);
    assert!(version.is_ok());
    assert_eq!(version.unwrap(), RUNTIME_MIN_VERSION);
}

/// WHEN agent supports v1.2+ only THEN negotiation fails (runtime max is 1.1).
#[test]
fn version_negotiation_fails_when_agent_requires_newer() {
    // Agent requires v1.2+
    let version = negotiate_version(1002, 1010);
    assert!(
        version.is_err(),
        "negotiation must fail when agent minimum exceeds runtime maximum"
    );
}
