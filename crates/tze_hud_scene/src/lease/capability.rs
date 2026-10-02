//! Capability scope enforcement and zone-publish gating.
//!
//! Implements:
//! - Requirement: Capability Vocabulary (configuration/spec.md §149)
//! - Requirement: Zone Publish Requires Active Lease (lease-governance/spec.md lines 213-224)
//! - Requirement: Lease Suspension Freezes Zone Publications (lines 226-233)
//! - Requirement: Lease Revocation Clears Zone Publications (lines 235-242)
//! - RFC 0001 §3.3: Live capability revocation from active leases
//!
//! ## Zone publish gating
//!
//! For resident agents, publishing to a zone requires all of:
//! 1. An ACTIVE lease (not SUSPENDED, ORPHANED, or terminal).
//! 2. `publish_zone:<zone_type>` capability (or `publish_zone:*`) for the target zone.
//! 3. The zone instance exists in the current active tab.
//!
//! Error mapping per spec:
//! | Condition                           | Error code                  |
//! |-------------------------------------|-----------------------------|
//! | No lease for namespace              | `LEASE_NOT_FOUND`           |
//! | Lease exists but not ACTIVE         | `LEASE_NOT_ACTIVE`          |
//! | Lease ACTIVE but capability missing | `CAPABILITY_NOT_GRANTED`    |
//! | Zone does not exist in active tab   | `ZONE_NOT_FOUND`            |
//! | Lease is SUSPENDED (safe mode)      | `SAFE_MODE_ACTIVE`          |
//!
//! ## Live capability revocation
//!
//! RFC 0001 §3.3 specifies that capability enforcement happens at mutation time against
//! the live capability scope, not merely at grant time. The [`revoke_capability_from_lease`]
//! function removes a capability from an active lease's scope at runtime. Future mutations
//! requiring that capability will be rejected with `CapabilityMissing`.

use crate::types::{Capability, Lease, LeaseState};

// ─── Zone-publish error codes ────────────────────────────────────────────────

/// Structured error returned by [`check_zone_publish`].
///
/// Maps 1-to-1 to the error codes in spec §Requirement: Zone Publish Requires
/// Active Lease (lines 213-224).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZonePublishError {
    /// No lease found for the requesting namespace.
    LeaseNotFound,
    /// Lease exists but is in a non-ACTIVE state (SUSPENDED, ORPHANED, terminal, …).
    LeaseNotActive,
    /// Lease is SUSPENDED — safe mode is active; new publishes blocked.
    SafeModeActive,
    /// Lease is ACTIVE but does not hold `publish_zone:<zone_type>` capability.
    CapabilityNotGranted,
    /// The named zone does not exist in the current active tab.
    ZoneNotFound,
}

impl std::fmt::Display for ZonePublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZonePublishError::LeaseNotFound => write!(f, "LEASE_NOT_FOUND"),
            ZonePublishError::LeaseNotActive => write!(f, "LEASE_NOT_ACTIVE"),
            ZonePublishError::SafeModeActive => write!(f, "SAFE_MODE_ACTIVE"),
            ZonePublishError::CapabilityNotGranted => write!(f, "CAPABILITY_NOT_GRANTED"),
            ZonePublishError::ZoneNotFound => write!(f, "ZONE_NOT_FOUND"),
        }
    }
}

impl std::error::Error for ZonePublishError {}

// ─── Capability check helpers ─────────────────────────────────────────────────

/// Returns `true` if `capabilities` grants the agent permission to publish to
/// `zone_name`.
///
/// Accepts either the wildcard `publish_zone:*` (grants all zones) or the
/// specific `publish_zone:<zone_name>` capability.
///
/// From spec §Requirement: Capability Vocabulary: `publish_zone:<zone_name>` and
/// `publish_zone:*` are both valid capability identifiers.
pub fn has_publish_zone_capability(capabilities: &[Capability], zone_name: &str) -> bool {
    capabilities.iter().any(|c| match c {
        Capability::PublishZone(name) => name == zone_name || name == "*",
        _ => false,
    })
}

/// Returns `true` if `capabilities` contains the given generic capability.
///
/// This is a convenience wrapper around the `Vec::contains` pattern; it exists
/// so callers do not need to import `Capability` variants directly.
pub fn has_capability(capabilities: &[Capability], cap: &Capability) -> bool {
    capabilities.contains(cap)
}

// ─── Zone publish gate ───────────────────────────────────────────────────────

/// Check whether a resident agent is allowed to publish to `zone_name`.
///
/// `lease` is the agent's current lease (if any — pass `None` to simulate
/// a missing lease).  `zone_registered` indicates whether the zone instance
/// exists in the current active tab.
///
/// Returns `Ok(())` when the publish may proceed, or a [`ZonePublishError`]
/// describing exactly why it was rejected.
///
/// # Order of checks (spec §Requirement: Zone Publish Requires Active Lease, lines 213-224)
///
/// 1. Missing lease → `LeaseNotFound`
/// 2. Lease is `Suspended` → `SafeModeActive` (spec §Requirement: Lease Suspension
///    Freezes Zone Publications, lines 226-233: "new publishes rejected with SAFE_MODE_ACTIVE")
/// 3. Lease is any other non-ACTIVE state → `LeaseNotActive`
/// 4. Lease is ACTIVE but lacks `publish_zone:<zone_name>` capability → `CapabilityNotGranted`
/// 5. Zone not registered in active tab → `ZoneNotFound`
pub fn check_zone_publish(
    lease: Option<&Lease>,
    zone_name: &str,
    zone_registered: bool,
) -> Result<(), ZonePublishError> {
    let lease = lease.ok_or(ZonePublishError::LeaseNotFound)?;

    match lease.state {
        LeaseState::Active => {} // proceed to capability check
        LeaseState::Suspended => {
            // Spec §Lease Suspension Freezes Zone Publications (line 227-228):
            // "New zone publishes under the suspended lease MUST be rejected with SAFE_MODE_ACTIVE."
            return Err(ZonePublishError::SafeModeActive);
        }
        _ => {
            // Orphaned, terminal states, etc.
            return Err(ZonePublishError::LeaseNotActive);
        }
    }

    if !has_publish_zone_capability(&lease.capabilities, zone_name) {
        return Err(ZonePublishError::CapabilityNotGranted);
    }

    if !zone_registered {
        return Err(ZonePublishError::ZoneNotFound);
    }

    Ok(())
}

// ─── Live capability revocation ──────────────────────────────────────────────

/// Error returned by [`revoke_capability_from_lease`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityRevocationError {
    /// The lease is in a terminal state (Revoked, Expired, Released, Denied).
    /// Live capability revocation is only meaningful on non-terminal leases.
    LeaseTerminal,
    /// The specified capability is not present in the lease's current scope.
    /// This is a no-op signal; callers may choose to treat this as success.
    CapabilityNotPresent,
}

impl std::fmt::Display for CapabilityRevocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapabilityRevocationError::LeaseTerminal => {
                write!(
                    f,
                    "LEASE_TERMINAL: capability revocation requires a non-terminal lease"
                )
            }
            CapabilityRevocationError::CapabilityNotPresent => {
                write!(
                    f,
                    "CAPABILITY_NOT_PRESENT: capability is not in the lease scope"
                )
            }
        }
    }
}

impl std::error::Error for CapabilityRevocationError {}

/// Remove `cap` from the capability scope of a live (non-terminal) lease.
///
/// # RFC 0001 §3.3
///
/// The spec requires that capability checks are enforced at mutation time against the
/// live scope — not just at grant time. This function implements the live revocation path:
/// after calling this, any attempt to use `cap` under `lease` will be rejected with
/// `CapabilityMissing`.
///
/// # Behavior
///
/// - If the lease is terminal (`Revoked`, `Expired`, `Released`, `Denied`) → `Err(LeaseTerminal)`.
/// - If `cap` is not currently in the scope → `Err(CapabilityNotPresent)`.
/// - Otherwise, removes the capability in-place and returns `Ok(capability_name_string)`.
///   The caller is responsible for emitting a [`LeaseEventKind::CapabilityRevoked`] audit
///   event and notifying the agent via `CapabilityNotice`.
///
/// # State preservation
///
/// The lease remains in its current state (e.g., `Active`). Only the capability scope
/// is narrowed. This is distinct from full lease revocation ([`Lease::revoke`]), which
/// transitions the lease to the `Revoked` terminal state and removes all tiles.
pub fn revoke_capability_from_lease(
    lease: &mut Lease,
    cap: &Capability,
) -> Result<String, CapabilityRevocationError> {
    if lease.state.is_terminal() {
        return Err(CapabilityRevocationError::LeaseTerminal);
    }
    // Find and remove the capability.  Use positional removal to preserve order.
    let pos = lease.capabilities.iter().position(|c| c == cap);
    match pos {
        None => Err(CapabilityRevocationError::CapabilityNotPresent),
        Some(idx) => {
            lease.capabilities.remove(idx);
            Ok(format!("{cap:?}"))
        }
    }
}

// ─── Lease-revocation zone cleanup ───────────────────────────────────────────

/// Determine which zone publish records should be cleared when a lease is
/// revoked or expired.
///
/// Returns `true` for each record whose `publisher_namespace` matches the
/// revoked lease's `namespace`.
///
/// From spec §Requirement: Lease Revocation Clears Zone Publications (lines 235-242):
/// > "When a lease is REVOKED or EXPIRED, all zone publications made under that
/// >  lease MUST be immediately cleared from the zone registry."
///
/// # Usage
/// ```ignore
/// zone_registry.active_publishes.retain(|_zone, records| {
///     records.retain(|r| !should_clear_on_revoke(&lease, r.publisher_namespace.as_str()));
///     true
/// });
/// ```
pub fn should_clear_on_revoke(lease: &Lease, publisher_namespace: &str) -> bool {
    lease.namespace == publisher_namespace
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Capability, Lease, LeaseState, RenewalPolicy, ResourceBudget, SceneId};

    fn make_lease(state: LeaseState, capabilities: Vec<Capability>) -> Lease {
        Lease {
            id: SceneId::new(),
            namespace: "agent.test".to_string(),
            session_id: SceneId::nil(),
            state,
            priority: 2,
            granted_at_ms: 0,
            ttl_ms: 60_000,
            renewal_policy: RenewalPolicy::default(),
            capabilities,
            resource_budget: ResourceBudget::default(),
            spatial_budget: Default::default(),
            suspended_at_ms: None,
            ttl_remaining_at_suspend_ms: None,
            disconnected_at_ms: None,
            grace_period_ms: 30_000,
        }
    }

    // ── has_publish_zone_capability ──────────────────────────────────────────

    #[test]
    fn publish_zone_specific_matches() {
        let caps = vec![Capability::PublishZone("subtitle".to_string())];
        assert!(has_publish_zone_capability(&caps, "subtitle"));
        assert!(!has_publish_zone_capability(&caps, "notifications"));
    }

    #[test]
    fn publish_zone_wildcard_matches_all() {
        let caps = vec![Capability::PublishZone("*".to_string())];
        assert!(has_publish_zone_capability(&caps, "subtitle"));
        assert!(has_publish_zone_capability(&caps, "notifications"));
        assert!(has_publish_zone_capability(&caps, "anything"));
    }

    #[test]
    fn publish_zone_no_capability_returns_false() {
        let caps = vec![Capability::CreateTiles];
        assert!(!has_publish_zone_capability(&caps, "subtitle"));
    }

    // ── check_zone_publish ───────────────────────────────────────────────────

    /// WHEN no lease exists THEN LEASE_NOT_FOUND.
    #[test]
    fn zone_publish_no_lease_returns_not_found() {
        let err = check_zone_publish(None, "subtitle", true).unwrap_err();
        assert_eq!(err, ZonePublishError::LeaseNotFound);
    }

    /// WHEN lease is SUSPENDED THEN SAFE_MODE_ACTIVE.
    ///
    /// Spec scenario (lines 219-220):
    /// "WHEN a resident agent attempts to publish to a zone while its lease is SUSPENDED
    ///  THEN the publish is rejected with LEASE_NOT_ACTIVE"
    /// Note: the spec text at line 219-220 says LEASE_NOT_ACTIVE but §Lease Suspension
    /// Freezes Zone Publications (line 227-228) says SAFE_MODE_ACTIVE. This module
    /// implements SAFE_MODE_ACTIVE per §7.2 which is the more specific requirement.
    #[test]
    fn zone_publish_suspended_lease_returns_safe_mode_active() {
        let lease = make_lease(
            LeaseState::Suspended,
            vec![Capability::PublishZone("subtitle".to_string())],
        );
        let err = check_zone_publish(Some(&lease), "subtitle", true).unwrap_err();
        assert_eq!(err, ZonePublishError::SafeModeActive);
    }

    /// WHEN lease is ORPHANED THEN LEASE_NOT_ACTIVE.
    #[test]
    fn zone_publish_orphaned_lease_returns_not_active() {
        let lease = make_lease(
            LeaseState::Orphaned,
            vec![Capability::PublishZone("subtitle".to_string())],
        );
        let err = check_zone_publish(Some(&lease), "subtitle", true).unwrap_err();
        assert_eq!(err, ZonePublishError::LeaseNotActive);
    }

    /// WHEN lease is REVOKED THEN LEASE_NOT_ACTIVE.
    #[test]
    fn zone_publish_revoked_lease_returns_not_active() {
        let lease = make_lease(
            LeaseState::Revoked,
            vec![Capability::PublishZone("subtitle".to_string())],
        );
        let err = check_zone_publish(Some(&lease), "subtitle", true).unwrap_err();
        assert_eq!(err, ZonePublishError::LeaseNotActive);
    }

    /// WHEN lease ACTIVE but lacks publish_zone:subtitle THEN CAPABILITY_NOT_GRANTED.
    ///
    /// Spec scenario (lines 222-224):
    /// "WHEN an agent holds an ACTIVE lease but lacks publish_zone:subtitle and
    ///  publishes to subtitle THEN the publish is rejected with CAPABILITY_NOT_GRANTED"
    #[test]
    fn zone_publish_active_missing_capability_returns_capability_not_granted() {
        let lease = make_lease(LeaseState::Active, vec![Capability::CreateTiles]);
        let err = check_zone_publish(Some(&lease), "subtitle", true).unwrap_err();
        assert_eq!(err, ZonePublishError::CapabilityNotGranted);
    }

    /// WHEN lease ACTIVE and has capability but zone not registered THEN ZONE_NOT_FOUND.
    #[test]
    fn zone_publish_missing_zone_returns_zone_not_found() {
        let lease = make_lease(
            LeaseState::Active,
            vec![Capability::PublishZone("subtitle".to_string())],
        );
        let err = check_zone_publish(Some(&lease), "subtitle", false).unwrap_err();
        assert_eq!(err, ZonePublishError::ZoneNotFound);
    }

    /// WHEN lease ACTIVE, has capability, zone registered THEN publish succeeds.
    #[test]
    fn zone_publish_happy_path() {
        let lease = make_lease(
            LeaseState::Active,
            vec![Capability::PublishZone("subtitle".to_string())],
        );
        assert!(check_zone_publish(Some(&lease), "subtitle", true).is_ok());
    }

    /// Wildcard capability allows publishing to any zone.
    #[test]
    fn zone_publish_wildcard_capability_grants_any_zone() {
        let lease = make_lease(
            LeaseState::Active,
            vec![Capability::PublishZone("*".to_string())],
        );
        assert!(check_zone_publish(Some(&lease), "subtitle", true).is_ok());
        assert!(check_zone_publish(Some(&lease), "notifications", true).is_ok());
    }

    // ── should_clear_on_revoke ───────────────────────────────────────────────

    /// Publications from the revoked lease's namespace are cleared.
    #[test]
    fn clear_on_revoke_matches_namespace() {
        let lease = make_lease(LeaseState::Revoked, vec![]);
        assert!(should_clear_on_revoke(&lease, "agent.test"));
        assert!(!should_clear_on_revoke(&lease, "agent.other"));
    }

    // ── revoke_capability_from_lease ─────────────────────────────────────────

    /// WHEN revoking a capability that exists in an active lease THEN it is removed
    /// and Ok(name) is returned.
    #[test]
    fn revoke_capability_removes_cap_from_active_lease() {
        let mut lease = make_lease(
            LeaseState::Active,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let result = revoke_capability_from_lease(&mut lease, &Capability::CreateTiles);
        assert!(result.is_ok(), "should succeed: {result:?}");
        assert!(
            !lease.capabilities.contains(&Capability::CreateTiles),
            "CreateTiles should be removed"
        );
        assert!(
            lease.capabilities.contains(&Capability::ModifyOwnTiles),
            "ModifyOwnTiles should remain"
        );
    }

    /// WHEN revoking a capability from a non-active (suspended) lease THEN the
    /// capability is still removed — the lease is not terminal.
    #[test]
    fn revoke_capability_works_on_suspended_lease() {
        let mut lease = make_lease(LeaseState::Suspended, vec![Capability::ManageTabs]);
        let result = revoke_capability_from_lease(&mut lease, &Capability::ManageTabs);
        assert!(
            result.is_ok(),
            "suspended lease is not terminal: {result:?}"
        );
        assert!(lease.capabilities.is_empty());
    }

    /// WHEN revoking a capability from a terminal (revoked) lease THEN LeaseTerminal
    /// error is returned.
    #[test]
    fn revoke_capability_from_revoked_lease_returns_terminal_error() {
        let mut lease = make_lease(LeaseState::Revoked, vec![Capability::CreateTiles]);
        let err = revoke_capability_from_lease(&mut lease, &Capability::CreateTiles).unwrap_err();
        assert_eq!(err, CapabilityRevocationError::LeaseTerminal);
        // Capability must remain unchanged (no side-effect on error).
        assert!(lease.capabilities.contains(&Capability::CreateTiles));
    }

    /// WHEN revoking a capability from a terminal (expired) lease THEN LeaseTerminal error.
    #[test]
    fn revoke_capability_from_expired_lease_returns_terminal_error() {
        let mut lease = make_lease(LeaseState::Expired, vec![Capability::CreateTiles]);
        let err = revoke_capability_from_lease(&mut lease, &Capability::CreateTiles).unwrap_err();
        assert_eq!(err, CapabilityRevocationError::LeaseTerminal);
    }

    /// WHEN revoking a capability that is NOT in the lease scope THEN
    /// CapabilityNotPresent error is returned.
    #[test]
    fn revoke_capability_not_present_returns_error() {
        let mut lease = make_lease(LeaseState::Active, vec![Capability::CreateTiles]);
        let err =
            revoke_capability_from_lease(&mut lease, &Capability::ModifyOwnTiles).unwrap_err();
        assert_eq!(err, CapabilityRevocationError::CapabilityNotPresent);
        // Scope must remain unchanged.
        assert_eq!(lease.capabilities, vec![Capability::CreateTiles]);
    }

    /// WHEN all capabilities are successively revoked THEN the scope becomes empty.
    #[test]
    fn revoke_all_capabilities_leaves_empty_scope() {
        let mut lease = make_lease(
            LeaseState::Active,
            vec![
                Capability::CreateTiles,
                Capability::ModifyOwnTiles,
                Capability::ManageTabs,
            ],
        );
        for cap in &[
            Capability::CreateTiles,
            Capability::ModifyOwnTiles,
            Capability::ManageTabs,
        ] {
            revoke_capability_from_lease(&mut lease, cap).expect("should succeed");
        }
        assert!(
            lease.capabilities.is_empty(),
            "scope should be empty after all revocations"
        );
        assert_eq!(lease.state, LeaseState::Active, "lease must remain Active");
    }

    /// WHEN revoking a PublishZone capability THEN zone publish is subsequently rejected.
    #[test]
    fn revoke_publish_zone_capability_blocks_zone_publish() {
        let mut lease = make_lease(
            LeaseState::Active,
            vec![Capability::PublishZone("subtitle".to_string())],
        );
        // Before revocation: publish succeeds.
        assert!(check_zone_publish(Some(&lease), "subtitle", true).is_ok());
        // Revoke the capability.
        revoke_capability_from_lease(&mut lease, &Capability::PublishZone("subtitle".to_string()))
            .expect("revocation should succeed");
        // After revocation: publish is rejected.
        let err = check_zone_publish(Some(&lease), "subtitle", true).unwrap_err();
        assert_eq!(err, ZonePublishError::CapabilityNotGranted);
    }
}
