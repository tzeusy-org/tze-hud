//! Authentication and agent identity for the session handshake.
//!
//! - Evaluating `AuthCredential` during `SessionInit` / `SessionResume`
//! - Resolving the credential to an agent and its `allow`-derived permissions
//!   ([`identify_session`])
//!
//! # V1 Auth Implementations
//!
//! Per RFC 0005 §1.4 and the v1-mandatory scope, two credential types are
//! fully implemented:
//!
//! - `PreSharedKeyCredential` — matched against the server PSK.
//! - `LocalSocketCredential` — accepted only when the peer address is a
//!   loopback address (`127.0.0.0/8` or `::1`).  Non-loopback peers are
//!   rejected with `AUTH_FAILED` (see `hud-1aswu.1`).
//!
//! `OauthTokenCredential` and `MtlsCredential` are schema-defined
//! (proto messages exist) but their implementations are v1-reserved; they
//! are rejected with `AUTH_FAILED` until a future release enables them.

use std::net::IpAddr;

use subtle::ConstantTimeEq;
use tze_hud_scene::config::{AgentDirectory, AgentIdentity, AuthRejection};

use crate::proto::session::{AuthCredential, auth_credential::Credential};

// Constant-time byte-level equality to resist timing side-channels.
//
// Backed by `subtle::ConstantTimeEq`, the de-facto-standard constant-time
// primitive (RustCrypto). For byte slices it short-circuits to a non-match
// when lengths differ (length is not secret for the PSK) and otherwise
// performs a branch-free fold over the bytes — exactly the semantics of the
// previous hand-rolled xor-fold, with the security-relevant primitive now
// maintained upstream.
//
// This is NOT a cryptographic HMAC; for v1 PSK the surface is local gRPC only.
fn ct_eq_str(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

// ─── Credential evaluation ────────────────────────────────────────────────────

/// Result of an authentication attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResult {
    /// Authentication succeeded.
    Accepted,
    /// Authentication failed. The reason string is sent in `SessionError`.
    Failed(String),
    /// The credential type is not yet implemented (v1-reserved).
    Unimplemented(String),
}

/// Evaluate a structured `AuthCredential` against the server configuration.
///
/// `psk` is the pre-shared key configured on the server.
///
/// `peer_addr` is the remote address of the connecting client, used to gate
/// `LocalSocketCredential` acceptance — only loopback peers are accepted
/// (hud-1aswu.1).  Pass `None` when the peer address is unavailable (e.g.
/// in unit tests); in that case `LocalSocketCredential` is rejected as a
/// conservative fallback.
pub fn evaluate_auth_credential(
    credential: &AuthCredential,
    psk: &str,
    peer_addr: Option<IpAddr>,
) -> AuthResult {
    match &credential.credential {
        Some(Credential::PreSharedKey(cred)) => {
            // Use branch-free comparison to resist timing side-channels.
            if ct_eq_str(&cred.key, psk) {
                AuthResult::Accepted
            } else {
                AuthResult::Failed("pre-shared key mismatch".to_string())
            }
        }
        Some(Credential::LocalSocket(_cred)) => {
            // Security fix (hud-1aswu.1): LocalSocketCredential is only valid
            // when the connection originates from loopback.  A non-loopback peer
            // (LAN, tailnet, etc.) presenting this credential is rejected with
            // AUTH_FAILED — the stable error code for authentication failures.
            //
            // `peer_addr = None` is treated as non-loopback (conservative).
            match peer_addr {
                Some(addr) if addr.is_loopback() => AuthResult::Accepted,
                Some(addr) => AuthResult::Failed(format!(
                    "LocalSocketCredential rejected: peer {addr} is not a loopback address; \
                     use PreSharedKeyCredential for non-local connections"
                )),
                None => AuthResult::Failed(
                    "LocalSocketCredential rejected: peer address unknown (non-loopback assumed); \
                     use PreSharedKeyCredential"
                        .to_string(),
                ),
            }
        }
        Some(Credential::OauthToken(_)) => {
            // v1-reserved: OauthTokenCredential schema exists but is not implemented.
            AuthResult::Unimplemented(
                "OauthTokenCredential is not implemented in v1; use PreSharedKeyCredential"
                    .to_string(),
            )
        }
        Some(Credential::Mtls(_)) => {
            // v1-reserved: MtlsCredential schema exists but is not implemented.
            AuthResult::Unimplemented(
                "MtlsCredential is not implemented in v1; use PreSharedKeyCredential".to_string(),
            )
        }
        None => {
            // Empty AuthCredential: treat as "no credential provided" — fail auth.
            AuthResult::Failed("no credential provided in AuthCredential".to_string())
        }
    }
}

/// Build a pre-shared-key `AuthCredential` for `SessionInit`.
pub fn psk_credential(key: impl Into<String>) -> AuthCredential {
    AuthCredential {
        credential: Some(Credential::PreSharedKey(
            crate::proto::session::PreSharedKeyCredential { key: key.into() },
        )),
    }
}

/// Authenticate a session from its structured `auth_credential`.
///
/// `legacy_psk` is the plain-string PSK still carried by `SessionResume`;
/// `SessionInit` passes an empty string.
///
/// `peer_addr` is forwarded to `evaluate_auth_credential` for
/// `LocalSocketCredential` loopback gating (hud-1aswu.1).
pub fn authenticate_session_init(
    auth_credential: Option<&AuthCredential>,
    legacy_psk: &str,
    server_psk: &str,
    peer_addr: Option<IpAddr>,
) -> AuthResult {
    // If a structured credential is provided, use it.
    if let Some(cred) = auth_credential {
        if cred.credential.is_some() {
            return evaluate_auth_credential(cred, server_psk, peer_addr);
        }
    }

    // Fall back to the deprecated plain-string PSK field.
    // Use branch-free comparison to resist timing side-channels.
    if ct_eq_str(legacy_psk, server_psk) {
        AuthResult::Accepted
    } else {
        AuthResult::Failed("invalid pre-shared key".to_string())
    }
}

/// Resolve a handshake credential to an agent identity.
///
/// A PSK credential (structured, or the legacy plain string on resume) is
/// resolved through [`AgentDirectory::resolve`]: a paired agent's PSK
/// identifies that agent. An accepted loopback `LocalSocketCredential` is
/// treated like the dev PSK ([`AgentDirectory::resolve_local`]), so it
/// identifies no one in production.
pub fn identify_session(
    agents: &AgentDirectory,
    auth_credential: Option<&AuthCredential>,
    legacy_psk: &str,
    claimed_agent_id: &str,
    peer_addr: Option<IpAddr>,
) -> Result<AgentIdentity, AuthRejection> {
    let key = match auth_credential.and_then(|c| c.credential.as_ref()) {
        Some(Credential::PreSharedKey(cred)) => cred.key.as_str(),
        Some(_) => {
            let cred = auth_credential.expect("credential checked above");
            // The PSK argument is unused for non-PSK credentials.
            return match evaluate_auth_credential(cred, "", peer_addr) {
                AuthResult::Accepted => agents.resolve_local(claimed_agent_id),
                AuthResult::Failed(message) => Err(AuthRejection {
                    code: "AUTH_FAILED",
                    message,
                    hint: String::new(),
                }),
                AuthResult::Unimplemented(message) => Err(AuthRejection {
                    code: "AUTH_FAILED",
                    message,
                    hint:
                        r#"{"supported_v1": ["PreSharedKeyCredential", "LocalSocketCredential"]}"#
                            .to_string(),
                }),
            };
        }
        None => legacy_psk,
    };
    agents.resolve(key, claimed_agent_id)
}

// ─── Protocol version negotiation (RFC 0005 §4.1) ────────────────────────────

/// Runtime's supported version range.
/// `version = major * 1000 + minor`.
pub const RUNTIME_MIN_VERSION: u32 = 1000; // v1.0
pub const RUNTIME_MAX_VERSION: u32 = 1001; // v1.1

/// Negotiate the protocol version between agent and runtime.
///
/// Returns the highest mutually supported version, or `Err` with an
/// `UNSUPPORTED_PROTOCOL_VERSION` message if no mutual version exists.
///
/// If the agent sends `min=0, max=0` (unset), we treat it as `min=1000, max=1000`
/// (v1.0 only, backward compatible).
pub fn negotiate_version(agent_min: u32, agent_max: u32) -> Result<u32, String> {
    // Treat 0 (unset) as v1.0 for backward compatibility.
    let a_min = if agent_min == 0 {
        RUNTIME_MIN_VERSION
    } else {
        agent_min
    };
    let a_max = if agent_max == 0 {
        RUNTIME_MIN_VERSION
    } else {
        agent_max
    };

    // Find the highest version in the intersection of [a_min, a_max] and [RUNTIME_MIN, RUNTIME_MAX].
    let low = a_min.max(RUNTIME_MIN_VERSION);
    let high = a_max.min(RUNTIME_MAX_VERSION);

    if low > high {
        Err(format!(
            "no mutual protocol version: agent supports {a_min}-{a_max}, \
             runtime supports {RUNTIME_MIN_VERSION}-{RUNTIME_MAX_VERSION}"
        ))
    } else {
        Ok(high) // pick the highest mutual version
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::session::{
        AuthCredential, LocalSocketCredential, PreSharedKeyCredential, auth_credential::Credential,
    };

    fn psk_credential(key: &str) -> AuthCredential {
        AuthCredential {
            credential: Some(Credential::PreSharedKey(PreSharedKeyCredential {
                key: key.to_string(),
            })),
        }
    }

    fn local_socket_credential() -> AuthCredential {
        AuthCredential {
            credential: Some(Credential::LocalSocket(LocalSocketCredential {
                socket_path: "/run/tze_hud.sock".to_string(),
                pid_hint: "1234".to_string(),
            })),
        }
    }

    // ── Loopback helpers for tests ─────────────────────────────────────────────

    fn loopback_v4() -> Option<IpAddr> {
        Some("127.0.0.1".parse().unwrap())
    }

    fn loopback_v6() -> Option<IpAddr> {
        Some("::1".parse().unwrap())
    }

    fn lan_peer() -> Option<IpAddr> {
        Some("192.168.1.42".parse().unwrap())
    }

    fn non_loopback_tailnet() -> Option<IpAddr> {
        Some("10.0.0.5".parse().unwrap())
    }

    fn loopback_v4_alt() -> Option<IpAddr> {
        // 127.0.0.2 — still in the 127.0.0.0/8 loopback range; IpAddr::is_loopback() returns true.
        Some("127.0.0.2".parse().unwrap())
    }

    fn ipv4_mapped_loopback() -> Option<IpAddr> {
        // ::ffff:127.0.0.1 — IPv4-mapped IPv6 address for the IPv4 loopback.
        // IpAddr::is_loopback() returns false for this form (only ::1 and 127.x.x.x qualify).
        // The runtime is fail-closed: it rejects this as non-loopback until a future
        // release adds explicit dual-stack normalisation (hud-stl9j).
        Some("::ffff:127.0.0.1".parse().unwrap())
    }

    // ── Auth credential tests ──────────────────────────────────────────────────

    #[test]
    fn test_psk_credential_success() {
        let cred = psk_credential("secret");
        assert_eq!(
            evaluate_auth_credential(&cred, "secret", loopback_v4()),
            AuthResult::Accepted
        );
    }

    #[test]
    fn test_psk_credential_failure() {
        let cred = psk_credential("wrong");
        match evaluate_auth_credential(&cred, "secret", loopback_v4()) {
            AuthResult::Failed(_) => {}
            other => panic!("Expected Failed, got: {other:?}"),
        }
    }

    /// LocalSocketCredential from loopback IPv4 peer is accepted.
    #[test]
    fn test_local_socket_credential_accepted_loopback_v4() {
        let cred = local_socket_credential();
        assert_eq!(
            evaluate_auth_credential(&cred, "secret", loopback_v4()),
            AuthResult::Accepted
        );
    }

    /// LocalSocketCredential from loopback IPv6 peer is accepted.
    #[test]
    fn test_local_socket_credential_accepted_loopback_v6() {
        let cred = local_socket_credential();
        assert_eq!(
            evaluate_auth_credential(&cred, "secret", loopback_v6()),
            AuthResult::Accepted
        );
    }

    /// LocalSocketCredential from a LAN (non-loopback) peer is rejected with AUTH_FAILED.
    /// Security fix: hud-1aswu.1.
    #[test]
    fn test_local_socket_credential_rejected_lan_peer() {
        let cred = local_socket_credential();
        match evaluate_auth_credential(&cred, "secret", lan_peer()) {
            AuthResult::Failed(msg) => {
                assert!(
                    msg.contains("not a loopback address"),
                    "rejection message must mention loopback: {msg}"
                );
            }
            other => panic!("Expected Failed for LAN peer with LocalSocket cred, got: {other:?}"),
        }
    }

    /// LocalSocketCredential with unknown peer address (None) is rejected conservatively.
    #[test]
    fn test_local_socket_credential_rejected_unknown_peer() {
        let cred = local_socket_credential();
        match evaluate_auth_credential(&cred, "secret", None) {
            AuthResult::Failed(msg) => {
                assert!(
                    msg.contains("peer address unknown"),
                    "rejection message must mention unknown peer: {msg}"
                );
            }
            other => {
                panic!("Expected Failed for unknown peer with LocalSocket cred, got: {other:?}")
            }
        }
    }

    /// LocalSocketCredential from a non-loopback tailnet/VPN peer (10.x.x.x) is rejected.
    ///
    /// Pins the hud-1aswu.1 rejection for a realistic attack surface: an agent on the
    /// same tailnet attempting to use a local-socket credential that is only valid for
    /// same-machine loopback connections.
    #[test]
    fn test_local_socket_credential_rejected_tailnet_peer() {
        let cred = local_socket_credential();
        match evaluate_auth_credential(&cred, "secret", non_loopback_tailnet()) {
            AuthResult::Failed(msg) => {
                assert!(
                    msg.contains("not a loopback address"),
                    "rejection message must mention loopback: {msg}"
                );
                // Error must include the actual peer address so operators can diagnose.
                assert!(
                    msg.contains("10.0.0.5"),
                    "rejection message must include the peer address: {msg}"
                );
            }
            other => panic!(
                "Expected AUTH_FAILED for tailnet peer with LocalSocket cred, got: {other:?}"
            ),
        }
    }

    /// LocalSocketCredential from `127.0.0.2` (loopback /8 range, not just 127.0.0.1) is accepted.
    ///
    /// Pins the `IpAddr::is_loopback()` contract: the entire 127.0.0.0/8 range is
    /// loopback per IANA, so 127.0.0.2, 127.1.0.1, etc., must all be accepted.
    /// This matters for container environments that alias multiple loopback addresses.
    #[test]
    fn test_local_socket_credential_accepted_loopback_127_0_0_2() {
        let cred = local_socket_credential();
        assert_eq!(
            evaluate_auth_credential(&cred, "secret", loopback_v4_alt()),
            AuthResult::Accepted,
            "127.0.0.2 is in the loopback /8 range and must be accepted"
        );
    }

    /// LocalSocketCredential from `::ffff:127.0.0.1` (IPv4-mapped loopback) is rejected.
    ///
    /// Pins the fail-closed behaviour: `IpAddr::is_loopback()` returns `false` for the
    /// IPv4-mapped form even though the underlying IPv4 address is loopback.
    /// The runtime currently rejects this conservatively (hud-stl9j).  If a future
    /// release normalises IPv4-mapped addresses before the loopback check, this test
    /// must be updated to reflect the new semantics.
    #[test]
    fn test_local_socket_credential_rejected_ipv4_mapped_loopback() {
        let cred = local_socket_credential();
        match evaluate_auth_credential(&cred, "secret", ipv4_mapped_loopback()) {
            AuthResult::Failed(_) => {
                // Correct: fail-closed for IPv4-mapped form (hud-stl9j).
                // If dual-stack normalisation is added this test will need updating.
            }
            other => panic!(
                "Expected AUTH_FAILED for ::ffff:127.0.0.1 (IPv4-mapped loopback, fail-closed), \
                 got: {other:?}"
            ),
        }
    }

    #[test]
    fn test_oauth_credential_unimplemented() {
        use crate::proto::session::OauthTokenCredential;
        let cred = AuthCredential {
            credential: Some(Credential::OauthToken(OauthTokenCredential {
                bearer_token: "token".to_string(),
                token_type: "Bearer".to_string(),
            })),
        };
        match evaluate_auth_credential(&cred, "secret", loopback_v4()) {
            AuthResult::Unimplemented(_) => {}
            other => panic!("Expected Unimplemented, got: {other:?}"),
        }
    }

    #[test]
    fn test_mtls_credential_unimplemented() {
        use crate::proto::session::MtlsCredential;
        let cred = AuthCredential {
            credential: Some(Credential::Mtls(MtlsCredential {
                client_certificate_der: vec![1, 2, 3],
                expected_san: "test".to_string(),
            })),
        };
        match evaluate_auth_credential(&cred, "secret", loopback_v4()) {
            AuthResult::Unimplemented(_) => {}
            other => panic!("Expected Unimplemented, got: {other:?}"),
        }
    }

    #[test]
    fn test_empty_credential_fails() {
        let cred = AuthCredential { credential: None };
        match evaluate_auth_credential(&cred, "secret", loopback_v4()) {
            AuthResult::Failed(_) => {}
            other => panic!("Expected Failed, got: {other:?}"),
        }
    }

    // ── authenticate_session_init tests ───────────────────────────────────────

    #[test]
    fn test_session_init_structured_cred_takes_precedence() {
        let cred = psk_credential("correct");
        // Even with wrong legacy PSK, structured cred with correct key should pass
        assert_eq!(
            authenticate_session_init(Some(&cred), "wrong-legacy", "correct", loopback_v4()),
            AuthResult::Accepted
        );
    }

    #[test]
    fn test_session_init_legacy_psk_fallback() {
        // No structured credential → falls back to the plain PSK (SessionResume.pre_shared_key)
        assert_eq!(
            authenticate_session_init(None, "correct", "correct", loopback_v4()),
            AuthResult::Accepted
        );
    }

    #[test]
    fn test_session_init_legacy_psk_fallback_failure() {
        match authenticate_session_init(None, "wrong", "correct", loopback_v4()) {
            AuthResult::Failed(_) => {}
            other => panic!("Expected Failed, got: {other:?}"),
        }
    }

    #[test]
    fn test_session_init_empty_structured_cred_uses_legacy() {
        // AuthCredential with no credential variant set → fall back to legacy field
        let empty_cred = AuthCredential { credential: None };
        assert_eq!(
            authenticate_session_init(Some(&empty_cred), "correct", "correct", loopback_v4()),
            AuthResult::Accepted
        );
    }

    // ── Version negotiation tests ─────────────────────────────────────────────

    #[test]
    fn test_version_negotiation_success() {
        // Agent supports 1000-1001, runtime supports 1000-1001 → pick 1001
        assert_eq!(negotiate_version(1000, 1001), Ok(1001));
    }

    #[test]
    fn test_version_negotiation_exact_match() {
        assert_eq!(negotiate_version(1000, 1000), Ok(1000));
    }

    #[test]
    fn test_version_negotiation_no_overlap() {
        // Agent supports 2000-2001, runtime supports 1000-1001 → fail
        assert!(negotiate_version(2000, 2001).is_err());
    }

    #[test]
    fn test_version_negotiation_unset_treated_as_v1() {
        // min=0, max=0 → treated as 1000-1000 → pick 1000
        assert_eq!(negotiate_version(0, 0), Ok(1000));
    }

    #[test]
    fn test_version_negotiation_agent_below_runtime() {
        // Agent only supports 999 which is below RUNTIME_MIN_VERSION=1000
        assert!(negotiate_version(900, 999).is_err());
    }
}
