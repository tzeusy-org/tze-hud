//! Authentication and capability negotiation for the session handshake.
//!
//! Implements RFC 0005 §1.4 (Authentication) and the capability gating
//! policy described in RFC 0005 §5.3. This module is responsible for:
//!
//! - Evaluating `AuthCredential` during `SessionInit` / `SessionResume`
//! - Filtering requested capabilities against an agent authorization policy
//! - Evaluating mid-session `CapabilityRequest` against the same policy
//! - Filtering initial subscriptions by granted capabilities
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

// ─── Subscription capability requirements ────────────────────────────────────

/// Returns the capability required to subscribe to the given subscription
/// category (RFC 0005 §7.1). Returns `None` for categories that are always
/// allowed regardless of capabilities (DEGRADATION_NOTICES, LEASE_CHANGES).
pub fn required_capability_for_subscription(category: &str) -> Option<&'static str> {
    match category {
        "SCENE_TOPOLOGY" => Some("read_scene_topology"),
        "INPUT_EVENTS" => Some("access_input_events"),
        "FOCUS_EVENTS" => Some("access_input_events"),
        "ZONE_EVENTS" => Some("publish_zone"), // publish_zone:<zone> in full spec
        "TELEMETRY_FRAMES" => Some("read_telemetry"),
        "ATTENTION_EVENTS" => Some("read_scene_topology"),
        "AGENT_EVENTS" => Some("subscribe_scene_events"),
        // Always subscribed; capability not required:
        "DEGRADATION_NOTICES" => None,
        "LEASE_CHANGES" => None,
        _ => None, // Unknown categories: allow by default (forward compat)
    }
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

/// Authenticate from a `SessionInit` message.
///
/// Checks the structured `auth_credential` field first; falls back to the
/// deprecated `pre_shared_key` string field for backward compatibility with
/// agents built before the `AuthCredential` oneof was added.
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

// ─── Capability policy (RFC 0005 §5.3) ───────────────────────────────────────

/// Authorization policy for a single agent.
///
/// In v1 the policy is derived from the agent's registration (configured
/// PSK-based identity → allowed capability set). In a future release this
/// will consult a per-agent config file or dynamic approval flow.
///
/// # V1 policy rules
///
/// - Any capability listed in `allowed` is grantable.
/// - Any capability NOT in `allowed` is denied.
/// - Partial grants are denied entirely (RFC 0005 §5.3 scenario 4).
#[derive(Debug, Clone)]
pub struct CapabilityPolicy {
    /// The full set of capabilities this agent is authorized to hold.
    /// An empty set means the agent has no special capabilities (guest).
    allowed: Vec<String>,
}

impl CapabilityPolicy {
    /// Create a policy that allows exactly the given set of capabilities.
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }

    /// Unrestricted policy — allows any capability.
    ///
    /// Used for PSK-authenticated agents in v1 where the PSK holder is
    /// implicitly trusted for all capabilities.
    pub fn unrestricted() -> Self {
        // Sentinel: `"*"` in `allowed` indicates an allow-all policy.
        // `is_unrestricted()` and `permits()` both rely on this `"*"` marker.
        Self {
            allowed: vec!["*".to_string()],
        }
    }

    /// Guest policy — no capabilities granted.
    pub fn guest() -> Self {
        Self {
            allowed: Vec::new(),
        }
    }

    /// Returns `true` if this policy is unrestricted (permits any capability).
    pub fn is_unrestricted(&self) -> bool {
        self.allowed.iter().any(|a| a == "*")
    }

    /// Returns `true` if this policy grants the given capability.
    fn permits(&self, capability: &str) -> bool {
        self.allowed.iter().any(|a| a == "*" || a == capability)
    }

    /// Evaluate a capability grant request (RFC 0005 §5.3).
    ///
    /// Returns `Ok(Vec<String>)` with the granted capabilities on success, or
    /// `Err(Vec<String>)` listing the unauthorized capabilities if any are
    /// denied (the entire request is denied on any partial failure).
    pub fn evaluate_capability_request(
        &self,
        requested: &[String],
    ) -> Result<Vec<String>, Vec<String>> {
        let denied: Vec<String> = requested
            .iter()
            .filter(|cap| !self.permits(cap))
            .cloned()
            .collect();

        if denied.is_empty() {
            Ok(requested.to_vec())
        } else {
            // Deny the entire request on partial failure (RFC 0005 §5.3 scenario 4).
            Err(denied)
        }
    }

    /// Filter a set of requested capabilities into (granted, denied) lists
    /// for reporting in `SessionEstablished`.
    ///
    /// Unlike `evaluate_capability_request`, this does NOT deny the entire
    /// batch on partial failure; it partitions the set. Used only at handshake
    /// where individual grants/denials are reported separately.
    pub fn partition_capabilities(&self, requested: &[String]) -> (Vec<String>, Vec<String>) {
        let mut granted = Vec::new();
        let mut denied = Vec::new();
        for cap in requested {
            if self.permits(cap) {
                granted.push(cap.clone());
            } else {
                denied.push(cap.clone());
            }
        }
        (granted, denied)
    }

    /// Derive an unrestricted capability policy.
    ///
    /// Used for legacy/dev fallback-unrestricted mode where identity is not
    /// constrained by per-agent registration.
    pub fn for_psk_agent() -> Self {
        Self::unrestricted()
    }
}

// ─── Canonical capability vocabulary (configuration/spec.md §Capability Vocabulary) ──

/// Error produced when an unrecognized capability name is encountered.
///
/// Carries the wire-level fields surfaced in `CONFIG_UNKNOWN_CAPABILITY` errors
/// (`unknown` + `hint`). This is a subset of the full Structured Validation
/// Error collection shape defined in configuration/spec.md §Requirement:
/// Structured Validation Error Collection (which additionally requires
/// `field_path`, `expected`, and `got`); those fields are not included here
/// because capability name validation happens at the wire layer before any
/// field-path context is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownCapabilityError {
    /// The unrecognized capability name.
    pub unknown: String,
    /// A hint naming the closest canonical replacement, if any.
    pub hint: String,
}

/// Static list of fixed (non-parameterized) canonical v1 capability names.
///
/// Source: configuration/spec.md Requirement: Capability Vocabulary (lines 149-164),
/// RFC 0006 §6.3 (canonical authority), RFC 0005 Round 14 (wire-format amendments).
pub const CANONICAL_FIXED_CAPS: &[&str] = &[
    "create_tiles",
    "modify_own_tiles",
    "manage_tabs",
    "upload_resource",
    "register_widget_asset",
    "read_scene_topology",
    "subscribe_scene_events",
    "overlay_privileges",
    "access_input_events",
    "high_priority_z_order",
    "exceed_default_budgets",
    "read_telemetry",
    "resident_mcp",
    "lease:priority:1",
    // publish_zone:* is a parameterized form but * is a valid literal suffix.
    "publish_zone:*",
];

/// Validate that every capability in `requested` is a canonical v1 name.
///
/// Returns `Ok(())` if all names are canonical, or `Err(Vec<UnknownCapabilityError>)`
/// listing each unrecognized name with a hint for the canonical replacement.
///
/// Recognized forms:
/// - Fixed names in `CANONICAL_FIXED_CAPS`
/// - `publish_zone:<zone_name>` (non-empty zone name)
/// - `publish_widget:<widget_name>` (non-empty widget name)
/// - `emit_scene_event:<event_name>` (non-empty event name)
///
/// Rejected forms include: pre-Round-14 names (`read_scene`, `receive_input`,
/// `zone_publish`), legacy names (`create_tile`, `update_tile`, `delete_tile`,
/// `create_node`, `update_node`, `delete_node`), camelCase, kebab-case, etc.
pub fn validate_canonical_capabilities(
    requested: &[String],
) -> Result<(), Vec<UnknownCapabilityError>> {
    let mut errors = Vec::new();
    for cap in requested {
        if !is_canonical_capability(cap) {
            errors.push(UnknownCapabilityError {
                unknown: cap.clone(),
                hint: canonical_hint(cap),
            });
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Returns `true` if `cap` is a canonical v1 capability name.
fn is_canonical_capability(cap: &str) -> bool {
    // Fixed names.
    if CANONICAL_FIXED_CAPS.contains(&cap) {
        return true;
    }
    // Parameterized: publish_zone:<non-empty>
    if let Some(rest) = cap.strip_prefix("publish_zone:") {
        return !rest.is_empty();
    }
    // Parameterized: publish_widget:<non-empty>
    if let Some(rest) = cap.strip_prefix("publish_widget:") {
        return !rest.is_empty();
    }
    // Parameterized: emit_scene_event:<non-empty>, but system. and scene. prefixes
    // are reserved per configuration/spec.md §Capability Vocabulary — those names
    // must be rejected with CONFIG_RESERVED_EVENT_PREFIX, not CONFIG_UNKNOWN_CAPABILITY.
    // We reject them here (returning false) so the caller can report them as unknown;
    // the session_server distinguishes the two error codes in its own validation step.
    if let Some(rest) = cap.strip_prefix("emit_scene_event:") {
        if rest.is_empty() {
            return false;
        }
        // Reserved prefixes: system. and scene.
        if rest.starts_with("system.") || rest.starts_with("scene.") {
            return false;
        }
        return true;
    }
    // lease:priority:<N> (any non-empty ASCII digit string, including 0)
    if let Some(rest) = cap.strip_prefix("lease:priority:") {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// Return a hint string for a non-canonical capability name, pointing to
/// the canonical replacement where known.
///
/// The hint JSON format matches the spec example:
/// `{"unknown": "createTiles", "hint": "did you mean create_tiles?"}`
fn canonical_hint(cap: &str) -> String {
    // Pre-Round-14 names revised by RFC 0005 Round 14 (policy-arbitration/spec.md §281-292).
    if cap == "receive_input" {
        return r#"did you mean "access_input_events"? (pre-Round-14 name superseded by RFC 0005 Round 14)"#.to_string();
    }
    if cap == "read_scene" {
        return r#"did you mean "read_scene_topology"? (pre-Round-14 name superseded by RFC 0005 Round 14)"#.to_string();
    }
    if cap.starts_with("zone_publish:") {
        let zone = cap.strip_prefix("zone_publish:").unwrap_or("*");
        return format!(
            r#"did you mean "publish_zone:{zone}"? (pre-Round-14 name superseded by RFC 0005 Round 14)"#
        );
    }
    // Reserved emit_scene_event prefixes: system. and scene. are not allowed.
    if let Some(rest) = cap.strip_prefix("emit_scene_event:") {
        if rest.starts_with("system.") || rest.starts_with("scene.") {
            return r#"emit_scene_event names with "system." or "scene." prefix are reserved; use a non-reserved event name (CONFIG_RESERVED_EVENT_PREFIX)"#.to_string();
        }
    }
    // Legacy single-object names (create_tile → create_tiles, etc.).
    if cap == "create_tile" {
        return r#"did you mean "create_tiles"? (legacy name; use plural canonical form)"#
            .to_string();
    }
    if cap == "update_tile" || cap == "delete_tile" {
        return r#"did you mean "modify_own_tiles"? (legacy name; use canonical form)"#.to_string();
    }
    if cap == "create_node" || cap == "update_node" || cap == "delete_node" {
        return r#"did you mean "modify_own_tiles"? (legacy node-level name; use canonical tile-level form)"#.to_string();
    }
    // Generic fallback.
    "unknown capability; see configuration/spec.md §Capability Vocabulary for the canonical v1 list"
        .to_string()
}

// ─── Subscription filtering ───────────────────────────────────────────────────

/// Filter an agent's requested initial subscriptions against their granted capabilities.
///
/// Returns `(active, denied)` where:
/// - `active` are subscriptions the agent is allowed to receive.
/// - `denied` are subscriptions that require a capability the agent wasn't granted.
pub fn filter_subscriptions(
    requested: &[String],
    granted_capabilities: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut active = Vec::new();
    let mut denied = Vec::new();

    for sub in requested {
        match required_capability_for_subscription(sub) {
            None => {
                // No capability required (or always subscribed) — allow.
                active.push(sub.clone());
            }
            Some(required) => {
                let has_cap = granted_capabilities
                    .iter()
                    .any(|c| c == "*" || c == required);
                if has_cap {
                    active.push(sub.clone());
                } else {
                    denied.push(sub.clone());
                }
            }
        }
    }

    (active, denied)
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
        // No structured credential → falls back to legacy pre_shared_key field
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

    // ── Capability policy tests ────────────────────────────────────────────────

    #[test]
    fn test_policy_unrestricted_allows_any() {
        let policy = CapabilityPolicy::unrestricted();
        assert!(policy.permits("create_tiles"));
        assert!(policy.permits("read_telemetry"));
        assert!(policy.permits("overlay_privileges"));
    }

    #[test]
    fn test_policy_guest_denies_all() {
        let policy = CapabilityPolicy::guest();
        assert!(!policy.permits("create_tiles"));
        assert!(!policy.permits("read_telemetry"));
    }

    #[test]
    fn test_policy_specific_allows_listed() {
        let policy = CapabilityPolicy::new(vec!["read_telemetry".to_string()]);
        assert!(policy.permits("read_telemetry"));
        assert!(!policy.permits("overlay_privileges"));
    }

    #[test]
    fn test_capability_request_all_authorized() {
        let policy = CapabilityPolicy::unrestricted();
        let result = policy.evaluate_capability_request(&["read_telemetry".to_string()]);
        assert_eq!(result, Ok(vec!["read_telemetry".to_string()]));
    }

    #[test]
    fn test_capability_request_unauthorized_denied() {
        let policy = CapabilityPolicy::guest();
        let result = policy.evaluate_capability_request(&["overlay_privileges".to_string()]);
        assert!(result.is_err());
    }

    /// Scenario: Partial grant of mixed capabilities is denied entirely
    /// (RFC 0005 §5.3 scenario 4)
    #[test]
    fn test_capability_request_partial_grant_denied_entirely() {
        let policy = CapabilityPolicy::new(vec!["read_telemetry".to_string()]);
        // read_telemetry authorized, overlay_privileges not — deny entire request
        let result = policy.evaluate_capability_request(&[
            "read_telemetry".to_string(),
            "overlay_privileges".to_string(),
        ]);
        match result {
            Err(denied) => {
                assert!(denied.contains(&"overlay_privileges".to_string()));
                // read_telemetry should NOT appear in the denied list
                // (the error lists only the unauthorized ones, not the whole request)
            }
            Ok(_) => panic!("Expected denial for mixed capabilities"),
        }
    }

    #[test]
    fn test_partition_capabilities() {
        let policy = CapabilityPolicy::new(vec![
            "read_telemetry".to_string(),
            "create_tiles".to_string(), // canonical: plural
        ]);
        let (granted, denied) = policy.partition_capabilities(&[
            "read_telemetry".to_string(),
            "overlay_privileges".to_string(),
            "create_tiles".to_string(), // canonical: plural
        ]);
        assert_eq!(granted, vec!["read_telemetry", "create_tiles"]);
        assert_eq!(denied, vec!["overlay_privileges"]);
    }

    // ── Subscription filtering tests ───────────────────────────────────────────

    /// Scenario: Denied subscription for missing capability
    /// (RFC 0005 §7.1 scenario)
    #[test]
    fn test_subscription_denied_without_capability() {
        let (active, denied) = filter_subscriptions(
            &["INPUT_EVENTS".to_string()],
            &[/* no access_input_events */],
        );
        assert!(active.is_empty());
        assert_eq!(denied, vec!["INPUT_EVENTS"]);
    }

    #[test]
    fn test_subscription_allowed_with_capability() {
        let (active, denied) = filter_subscriptions(
            &["INPUT_EVENTS".to_string()],
            &["access_input_events".to_string()],
        );
        assert_eq!(active, vec!["INPUT_EVENTS"]);
        assert!(denied.is_empty());
    }

    #[test]
    fn test_subscription_always_allowed() {
        // DEGRADATION_NOTICES and LEASE_CHANGES need no capability
        let (active, denied) = filter_subscriptions(
            &[
                "DEGRADATION_NOTICES".to_string(),
                "LEASE_CHANGES".to_string(),
            ],
            &[/* no capabilities */],
        );
        assert_eq!(active.len(), 2);
        assert!(denied.is_empty());
    }

    #[test]
    fn test_subscription_unrestricted_allows_all() {
        let (active, denied) = filter_subscriptions(
            &[
                "SCENE_TOPOLOGY".to_string(),
                "INPUT_EVENTS".to_string(),
                "TELEMETRY_FRAMES".to_string(),
            ],
            &["*".to_string()],
        );
        assert_eq!(active.len(), 3);
        assert!(denied.is_empty());
    }

    #[test]
    fn test_subscription_mixed_capabilities() {
        let (active, denied) = filter_subscriptions(
            &[
                "SCENE_TOPOLOGY".to_string(),      // requires read_scene_topology
                "INPUT_EVENTS".to_string(),        // requires access_input_events
                "DEGRADATION_NOTICES".to_string(), // always allowed
            ],
            &["read_scene_topology".to_string()], // only has read_scene_topology
        );
        assert!(active.contains(&"SCENE_TOPOLOGY".to_string()));
        assert!(active.contains(&"DEGRADATION_NOTICES".to_string()));
        assert!(denied.contains(&"INPUT_EVENTS".to_string()));
    }

    // ── Canonical capability validation tests ──────────────────────────────────

    /// Scenario: Valid capability grants accepted
    /// (configuration/spec.md Requirement: Capability Vocabulary, line 154-156)
    #[test]
    fn test_canonical_caps_all_valid() {
        let caps = vec![
            "create_tiles".to_string(),
            "modify_own_tiles".to_string(),
            "manage_tabs".to_string(),
            "upload_resource".to_string(),
            "register_widget_asset".to_string(),
            "read_scene_topology".to_string(),
            "subscribe_scene_events".to_string(),
            "overlay_privileges".to_string(),
            "access_input_events".to_string(),
            "high_priority_z_order".to_string(),
            "exceed_default_budgets".to_string(),
            "read_telemetry".to_string(),
            "resident_mcp".to_string(),
            "publish_zone:subtitle".to_string(),
            "publish_zone:*".to_string(),
            "emit_scene_event:doorbell.ring".to_string(),
            "lease:priority:1".to_string(),
        ];
        assert!(validate_canonical_capabilities(&caps).is_ok());
    }

    /// Scenario: Non-canonical capability name rejected
    /// (configuration/spec.md Requirement: Capability Vocabulary, line 162-164)
    #[test]
    fn test_legacy_create_tile_rejected() {
        let caps = vec!["create_tile".to_string()];
        let err = validate_canonical_capabilities(&caps).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].unknown, "create_tile");
        assert!(err[0].hint.contains("create_tiles"));
    }

    /// Scenario: Pre-Round-14 name receive_input rejected with hint
    /// (policy-arbitration/spec.md §281-292)
    #[test]
    fn test_pre_round14_receive_input_rejected() {
        let caps = vec!["receive_input".to_string()];
        let err = validate_canonical_capabilities(&caps).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].unknown, "receive_input");
        assert!(err[0].hint.contains("access_input_events"));
    }

    /// Scenario: Pre-Round-14 name read_scene rejected with hint
    #[test]
    fn test_pre_round14_read_scene_rejected() {
        let caps = vec!["read_scene".to_string()];
        let err = validate_canonical_capabilities(&caps).unwrap_err();
        assert_eq!(err[0].unknown, "read_scene");
        assert!(err[0].hint.contains("read_scene_topology"));
    }

    /// Scenario: Pre-Round-14 name zone_publish rejected with hint
    #[test]
    fn test_pre_round14_zone_publish_rejected() {
        let caps = vec!["zone_publish:subtitle".to_string()];
        let err = validate_canonical_capabilities(&caps).unwrap_err();
        assert_eq!(err[0].unknown, "zone_publish:subtitle");
        assert!(err[0].hint.contains("publish_zone:subtitle"));
    }

    /// Multiple legacy names in one request → all reported.
    #[test]
    fn test_multiple_unknown_caps_reported() {
        let caps = vec![
            "create_tile".to_string(),
            "receive_input".to_string(),
            "read_scene_topology".to_string(), // valid — should not appear in errors
        ];
        let err = validate_canonical_capabilities(&caps).unwrap_err();
        assert_eq!(err.len(), 2);
        let unknown: Vec<&str> = err.iter().map(|e| e.unknown.as_str()).collect();
        assert!(unknown.contains(&"create_tile"));
        assert!(unknown.contains(&"receive_input"));
    }

    /// Empty capability list is valid.
    #[test]
    fn test_empty_capabilities_valid() {
        assert!(validate_canonical_capabilities(&[]).is_ok());
    }

    /// publish_zone with empty zone name is invalid.
    #[test]
    fn test_publish_zone_empty_suffix_invalid() {
        let caps = vec!["publish_zone:".to_string()];
        assert!(validate_canonical_capabilities(&caps).is_err());
    }

    /// publish_widget with non-empty widget name is valid.
    #[test]
    fn test_publish_widget_valid() {
        let caps = vec!["publish_widget:gauge".to_string()];
        assert!(validate_canonical_capabilities(&caps).is_ok());
    }

    /// publish_widget with empty widget name is invalid.
    #[test]
    fn test_publish_widget_empty_suffix_invalid() {
        let caps = vec!["publish_widget:".to_string()];
        assert!(validate_canonical_capabilities(&caps).is_err());
    }

    /// emit_scene_event with non-empty name is valid.
    #[test]
    fn test_emit_scene_event_valid() {
        let caps = vec!["emit_scene_event:my.event".to_string()];
        assert!(validate_canonical_capabilities(&caps).is_ok());
    }

    /// camelCase variant is rejected.
    #[test]
    fn test_camel_case_rejected() {
        let caps = vec!["createTiles".to_string()];
        let err = validate_canonical_capabilities(&caps).unwrap_err();
        assert!(!err[0].hint.is_empty());
    }

    /// kebab-case variant is rejected.
    #[test]
    fn test_kebab_case_rejected() {
        let caps = vec!["create-tiles".to_string()];
        assert!(validate_canonical_capabilities(&caps).is_err());
    }

    /// emit_scene_event with system. prefix is rejected (CONFIG_RESERVED_EVENT_PREFIX path).
    /// (configuration/spec.md §Capability Vocabulary: "system." and "scene." prefixes are reserved)
    #[test]
    fn test_emit_scene_event_system_prefix_rejected() {
        let caps = vec!["emit_scene_event:system.shutdown".to_string()];
        let err = validate_canonical_capabilities(&caps).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].unknown, "emit_scene_event:system.shutdown");
        assert!(
            err[0].hint.contains("reserved"),
            "hint must mention reserved prefix"
        );
    }

    /// emit_scene_event with scene. prefix is rejected (CONFIG_RESERVED_EVENT_PREFIX path).
    #[test]
    fn test_emit_scene_event_scene_prefix_rejected() {
        let caps = vec!["emit_scene_event:scene.refresh".to_string()];
        let err = validate_canonical_capabilities(&caps).unwrap_err();
        assert_eq!(err[0].unknown, "emit_scene_event:scene.refresh");
        assert!(
            err[0].hint.contains("reserved"),
            "hint must mention reserved prefix"
        );
    }

    /// emit_scene_event with empty event name is rejected.
    #[test]
    fn test_emit_scene_event_empty_suffix_rejected() {
        let caps = vec!["emit_scene_event:".to_string()];
        assert!(validate_canonical_capabilities(&caps).is_err());
    }
}
