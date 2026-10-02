//! Handshake authentication and identity tests.
//!
//! Credential evaluation, PSK → agent identity resolution, and version
//! negotiation.

use std::net::IpAddr;

use tze_hud_protocol::auth::AuthResult;
use tze_hud_protocol::auth::{
    RUNTIME_MAX_VERSION, RUNTIME_MIN_VERSION, authenticate_session_init, identify_session,
    negotiate_version,
};
use tze_hud_protocol::proto::session::auth_credential::Credential;
use tze_hud_protocol::proto::session::{
    AuthCredential, LocalSocketCredential, PreSharedKeyCredential,
};
use tze_hud_scene::config::AgentDirectory;

fn loopback() -> Option<IpAddr> {
    Some("127.0.0.1".parse().unwrap())
}

// ─── Identity ────────────────────────────────────────────────────────────────

fn directory() -> AgentDirectory {
    let mut dir = AgentDirectory {
        runtime_psk: "runtime".to_string(),
        ..Default::default()
    };
    dir.agent_psks
        .insert("claude".to_string(), "claude-psk".to_string());
    dir.permissions.insert(
        "claude".to_string(),
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

/// An agent's own PSK identifies it and yields its allow-derived permissions.
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

/// An agent with its own PSK cannot be claimed with the runtime PSK.
#[test]
fn runtime_psk_cannot_claim_agent_with_own_psk() {
    assert!(identify_session(&directory(), Some(&psk("runtime")), "", "claude", None).is_err());
}

/// An agent without a table gets the fallback permissions (none here).
#[test]
fn unconfigured_agent_gets_fallback_permissions() {
    let id = identify_session(&directory(), Some(&psk("runtime")), "", "stranger", None).unwrap();
    assert!(id.permissions.is_empty());
}

/// Loopback LocalSocketCredential is treated like the runtime PSK.
#[test]
fn local_socket_from_loopback_resolves_claimed_agent() {
    let cred = AuthCredential {
        credential: Some(Credential::LocalSocket(LocalSocketCredential::default())),
    };
    let id = identify_session(&directory(), Some(&cred), "", "stranger", loopback()).unwrap();
    assert_eq!(id.agent_id, "stranger");
}

// ─── Authentication ───────────────────────────────────────────────────────────

/// WHEN PSK matches server PSK THEN authentication succeeds.
#[test]
fn psk_authentication_succeeds_with_correct_key() {
    let server_psk = "secret-key-abc";
    let cred = AuthCredential {
        credential: Some(Credential::PreSharedKey(PreSharedKeyCredential {
            key: "secret-key-abc".to_string(),
        })),
    };
    let result = authenticate_session_init(Some(&cred), "", server_psk, loopback());
    assert_eq!(result, AuthResult::Accepted);
}

/// WHEN PSK does not match server PSK THEN authentication fails.
#[test]
fn psk_authentication_fails_with_wrong_key() {
    let server_psk = "secret-key-abc";
    let cred = AuthCredential {
        credential: Some(Credential::PreSharedKey(PreSharedKeyCredential {
            key: "wrong-key".to_string(),
        })),
    };
    let result = authenticate_session_init(Some(&cred), "", server_psk, loopback());
    assert!(
        matches!(result, AuthResult::Failed(_)),
        "wrong PSK must fail authentication"
    );
}

/// WHEN local socket credential provided from loopback peer THEN authentication accepted.
#[test]
fn local_socket_authentication_accepted_from_loopback() {
    let cred = AuthCredential {
        credential: Some(Credential::LocalSocket(LocalSocketCredential {
            socket_path: "/run/tze_hud.sock".to_string(),
            pid_hint: "1234".to_string(),
        })),
    };
    let result = authenticate_session_init(Some(&cred), "", "server-psk", loopback());
    assert_eq!(
        result,
        AuthResult::Accepted,
        "local socket credential from loopback must be accepted"
    );
}

/// WHEN local socket credential provided from non-loopback peer THEN authentication rejected.
/// Security fix: hud-1aswu.1 — reject LocalSocket for non-loopback peers.
#[test]
fn local_socket_authentication_rejected_from_lan_peer() {
    let cred = AuthCredential {
        credential: Some(Credential::LocalSocket(LocalSocketCredential {
            socket_path: "/run/tze_hud.sock".to_string(),
            pid_hint: "1234".to_string(),
        })),
    };
    let lan_peer: Option<IpAddr> = Some("10.0.0.5".parse().unwrap());
    let result = authenticate_session_init(Some(&cred), "", "server-psk", lan_peer);
    assert!(
        matches!(result, AuthResult::Failed(_)),
        "local socket credential from LAN peer must be rejected with AUTH_FAILED"
    );
}

/// WHEN legacy PSK field used (no auth_credential) THEN falls back to legacy check.
#[test]
fn legacy_psk_fallback_accepted() {
    let server_psk = "legacy-key";
    // No auth_credential — falls back to the plain PSK (SessionResume.pre_shared_key)
    let result = authenticate_session_init(None, "legacy-key", server_psk, loopback());
    assert_eq!(result, AuthResult::Accepted);
}

/// WHEN legacy PSK field wrong THEN fails.
#[test]
fn legacy_psk_fallback_rejected() {
    let server_psk = "legacy-key";
    let result = authenticate_session_init(None, "wrong-legacy-key", server_psk, loopback());
    assert!(matches!(result, AuthResult::Failed(_)));
}

/// WHEN empty auth credential provided THEN fails.
#[test]
fn empty_auth_credential_fails() {
    let cred = AuthCredential { credential: None };
    let result = authenticate_session_init(Some(&cred), "", "server-psk", loopback());
    assert!(
        matches!(result, AuthResult::Failed(_)),
        "empty AuthCredential must fail authentication"
    );
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
