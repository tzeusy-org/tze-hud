//! Post-revocation resource cleanup.
//!
//! Implements spec §Requirement: Post-Revocation Resource Cleanup (lines 253–260):
//!
//! On budget-driven revocation the compositor MUST:
//! 1. Transition all session leases to REVOKED.
//! 2. Send `LeaseResponse` with `revoke_reason = BUDGET_POLICY`.
//! 3. Mark tiles for removal.
//! 4. Bypass the grace period entirely.
//! 5. Free all resources after a 100 ms delay (to allow `LeaseResponse` delivery).
//!
//! Post-revocation resource footprint MUST be zero.
//!
//! # Relationship to grace period
//!
//! Budget-driven revocation (`RevokeReason::BudgetPolicy`) skips the orphan
//! grace window.  The lease transitions directly to REVOKED; the scene-graph
//! caller must invoke `remove_tiles_for_lease` (possibly after a 100 ms delay
//! to allow `LeaseResponse` delivery) rather than entering the orphan path.
//!
//! # Relationship to zone publications
//!
//! Spec §Requirement: Lease Revocation Clears Zone Publications (lines 235–242):
//! When a lease is REVOKED or EXPIRED, all zone publications made under that
//! lease MUST be cleared from the zone registry.  `ZonePublicationSweep` models
//! this operation.

use crate::lease::RevokeReason;
use crate::types::SceneId;

// ─── Post-revocation delay ───────────────────────────────────────────────────

/// Minimum delay (ms) between sending `LeaseResponse{revoke_reason=BUDGET_POLICY}`
/// and freeing compositor resources.
///
/// Spec line 254: "free all resources after a 100ms delay (to allow LeaseResponse
/// delivery)".  The delay is a lower bound; the compositor may wait longer.
pub const POST_REVOCATION_FREE_DELAY_MS: u64 = 100;

// ─── RevocationKind ──────────────────────────────────────────────────────────

/// Classification of why a revocation was triggered.
///
/// Determines whether the grace period is bypassed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevocationKind {
    /// Budget-policy enforcement (throttle 30s, critical OOM, repeated violations).
    ///
    /// **Bypasses the grace period entirely** per spec line 254.
    BudgetPolicy,
    /// Viewer explicitly dismissed the tile/session.
    ViewerDismissed,
    /// Safe-mode suspension timed out (> 300 000 ms).
    SuspensionTimeout,
    /// Unspecified runtime-initiated revocation.
    Other,
}

impl RevocationKind {
    /// Returns `true` if this revocation kind bypasses the grace period.
    ///
    /// Only `BudgetPolicy` bypasses grace.  All other kinds allow the normal
    /// disconnect/orphan path.
    pub fn bypasses_grace_period(self) -> bool {
        matches!(self, RevocationKind::BudgetPolicy)
    }

    /// Map to the wire-protocol `RevokeReason`.
    pub fn to_revoke_reason(self) -> RevokeReason {
        match self {
            RevocationKind::BudgetPolicy => RevokeReason::BudgetPolicy,
            RevocationKind::ViewerDismissed => RevokeReason::ViewerDismissed,
            RevocationKind::SuspensionTimeout => RevokeReason::SuspensionTimeout,
            RevocationKind::Other => RevokeReason::Other,
        }
    }
}

// ─── PostRevocationCleanupSpec ───────────────────────────────────────────────

/// Describes a pending post-revocation cleanup operation.
///
/// Created by the session layer when a lease is revoked.  The scene graph
/// executes the cleanup after `free_after_ms` have elapsed.
///
/// # Example flow
///
/// ```ignore
/// // 1. Runtime detects budget violation → triggers revocation
/// let spec = PostRevocationCleanupSpec {
///     lease_id,
///     session_namespace: "agent.foo".into(),
///     kind: RevocationKind::BudgetPolicy,
///     revoked_at_ms: now_ms,
///     free_after_ms: POST_REVOCATION_FREE_DELAY_MS,
/// };
///
/// // 2. Send LeaseResponse{revoke_reason = BUDGET_POLICY} over gRPC
/// //    (out-of-scope for this crate — handled by the session layer)
///
/// // 3. After free_after_ms, execute cleanup:
/// if spec.is_ready_to_free(now_ms) {
///     scene.remove_all_tiles_for_lease(spec.lease_id);
///     // post-revocation resource footprint is now zero
/// }
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct PostRevocationCleanupSpec {
    /// The revoked lease.
    pub lease_id: SceneId,
    /// Agent namespace for diagnostic messages.
    pub session_namespace: String,
    /// Why the lease was revoked.
    pub kind: RevocationKind,
    /// Wall-clock time when the lease was transitioned to REVOKED (ms).
    pub revoked_at_ms: u64,
    /// Minimum delay before compositor resources may be freed (ms).
    ///
    /// Defaults to `POST_REVOCATION_FREE_DELAY_MS`.
    pub free_after_ms: u64,
}

impl PostRevocationCleanupSpec {
    /// Create a new `PostRevocationCleanupSpec` with the default free delay.
    pub fn new(
        lease_id: SceneId,
        session_namespace: impl Into<String>,
        kind: RevocationKind,
        revoked_at_ms: u64,
    ) -> Self {
        Self {
            lease_id,
            session_namespace: session_namespace.into(),
            kind,
            revoked_at_ms,
            free_after_ms: POST_REVOCATION_FREE_DELAY_MS,
        }
    }

    /// Returns `true` when it is safe to free compositor resources.
    ///
    /// Spec line 254: resources freed after ≥ 100 ms to allow `LeaseResponse`
    /// delivery.
    pub fn is_ready_to_free(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.revoked_at_ms) >= self.free_after_ms
    }

    /// Milliseconds remaining until it is safe to free resources (0 if ready).
    pub fn ms_until_ready(&self, now_ms: u64) -> u64 {
        let elapsed = now_ms.saturating_sub(self.revoked_at_ms);
        self.free_after_ms.saturating_sub(elapsed)
    }

    /// Returns `true` if this revocation bypasses the reconnect grace period.
    pub fn bypasses_grace_period(&self) -> bool {
        self.kind.bypasses_grace_period()
    }
}

// ─── ZonePublicationSweep ────────────────────────────────────────────────────

/// Models the zone-publication clearing that occurs when a lease is
/// REVOKED or EXPIRED.
///
/// Spec §Requirement: Lease Revocation Clears Zone Publications (lines 235–242):
/// "When a lease is REVOKED or EXPIRED, all zone publications made under that
/// lease MUST be immediately cleared from the zone registry."
///
/// The actual clearing is done by `SceneGraph::clear_zone_publications_for_namespace`
/// (added in `graph.rs`).  This type captures *what* needs to be cleared.
#[derive(Clone, Debug, PartialEq)]
pub struct ZonePublicationSweep {
    /// The lease whose publications must be cleared.
    pub lease_id: SceneId,
    /// Agent namespace whose publications to remove.
    pub namespace: String,
    /// Terminal state that triggered the sweep (REVOKED or EXPIRED).
    pub terminal_state: crate::types::LeaseState,
}

impl ZonePublicationSweep {
    /// Create a new sweep descriptor.
    pub fn new(
        lease_id: SceneId,
        namespace: impl Into<String>,
        terminal_state: crate::types::LeaseState,
    ) -> Self {
        Self {
            lease_id,
            namespace: namespace.into(),
            terminal_state,
        }
    }

    /// Returns `true` if this sweep should unconditionally clear zone
    /// publications. Returns `true` for all terminal states (REVOKED, EXPIRED,
    /// DENIED, RELEASED); returns `false` for non-terminal states (ACTIVE,
    /// SUSPENDED, ORPHANED, REQUESTED).
    ///
    /// The spec (lines 235–242) specifically names REVOKED and EXPIRED as
    /// triggers; DENIED and RELEASED are also terminal and imply the lease
    /// never produced zone publications, so clearing is safe and consistent.
    pub fn should_clear(&self) -> bool {
        self.terminal_state.is_terminal()
    }
}

// ─── CleanupResult ───────────────────────────────────────────────────────────

/// Result of executing a `PostRevocationCleanupSpec`.
///
/// After cleanup, the resource footprint MUST be zero (spec line 260).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct CleanupResult {
    /// Number of tiles removed.
    pub tiles_removed: usize,
    /// Number of nodes freed.
    pub nodes_freed: usize,
    /// Number of zone publications cleared.
    pub zone_publications_cleared: usize,
    /// Whether resource footprint is confirmed zero.
    pub zero_footprint: bool,
}

impl CleanupResult {
    /// Assert zero footprint. Returns `Ok(())` if footprint is zero.
    pub fn assert_zero_footprint(&self) -> Result<(), String> {
        if self.zero_footprint {
            Ok(())
        } else {
            Err(format!(
                "non-zero post-revocation footprint: {} tiles, {} nodes, {} zone publications",
                self.tiles_removed, self.nodes_freed, self.zone_publications_cleared
            ))
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SceneId;

    fn dummy_lease() -> SceneId {
        SceneId::new()
    }

    // ── RevocationKind ──────────────────────────────────────────────────────

    /// BudgetPolicy revocation bypasses grace period.
    #[test]
    fn budget_policy_bypasses_grace_period() {
        assert!(RevocationKind::BudgetPolicy.bypasses_grace_period());
    }

    /// Other revocation kinds do NOT bypass grace period.
    #[test]
    fn non_budget_revocations_do_not_bypass_grace() {
        for kind in [
            RevocationKind::ViewerDismissed,
            RevocationKind::SuspensionTimeout,
            RevocationKind::Other,
        ] {
            assert!(
                !kind.bypasses_grace_period(),
                "{kind:?} should not bypass grace period"
            );
        }
    }

    /// RevokeReason mapping is correct.
    #[test]
    fn revocation_kind_maps_to_revoke_reason() {
        assert_eq!(
            RevocationKind::BudgetPolicy.to_revoke_reason(),
            RevokeReason::BudgetPolicy
        );
        assert_eq!(
            RevocationKind::ViewerDismissed.to_revoke_reason(),
            RevokeReason::ViewerDismissed
        );
    }

    // ── PostRevocationCleanupSpec ───────────────────────────────────────────

    /// WHEN 100ms have elapsed THEN is_ready_to_free returns true.
    #[test]
    fn cleanup_spec_ready_after_100ms() {
        let spec = PostRevocationCleanupSpec::new(
            dummy_lease(),
            "agent.foo",
            RevocationKind::BudgetPolicy,
            1_000,
        );
        assert!(!spec.is_ready_to_free(1_099), "not ready at 99ms");
        assert!(spec.is_ready_to_free(1_100), "ready at 100ms");
        assert!(spec.is_ready_to_free(2_000), "ready after 100ms");
    }

    /// WHEN revoked_at_ms = t, now = t + 50 THEN ms_until_ready = 50.
    #[test]
    fn cleanup_spec_ms_until_ready() {
        let spec = PostRevocationCleanupSpec::new(
            dummy_lease(),
            "agent.bar",
            RevocationKind::BudgetPolicy,
            500,
        );
        assert_eq!(spec.ms_until_ready(550), 50);
        assert_eq!(spec.ms_until_ready(600), 0);
        assert_eq!(spec.ms_until_ready(700), 0); // saturates at 0
    }

    /// BudgetPolicy cleanup spec bypasses grace period.
    #[test]
    fn cleanup_spec_budget_policy_bypasses_grace() {
        let spec = PostRevocationCleanupSpec::new(
            dummy_lease(),
            "agent.baz",
            RevocationKind::BudgetPolicy,
            0,
        );
        assert!(spec.bypasses_grace_period());
    }

    /// ViewerDismissed cleanup spec does NOT bypass grace period.
    #[test]
    fn cleanup_spec_viewer_dismissed_does_not_bypass_grace() {
        let spec = PostRevocationCleanupSpec::new(
            dummy_lease(),
            "agent.qux",
            RevocationKind::ViewerDismissed,
            0,
        );
        assert!(!spec.bypasses_grace_period());
    }

    // ── ZonePublicationSweep ────────────────────────────────────────────────

    /// WHEN lease is terminal THEN ZonePublicationSweep.should_clear returns true.
    #[test]
    fn zone_sweep_should_clear_for_terminal_states() {
        use crate::types::LeaseState;
        for state in [
            LeaseState::Revoked,
            LeaseState::Expired,
            LeaseState::Released,
            LeaseState::Denied,
        ] {
            let sweep = ZonePublicationSweep::new(dummy_lease(), "ns", state);
            assert!(sweep.should_clear(), "{state:?} must trigger zone clear");
        }
    }

    /// WHEN lease is non-terminal THEN ZonePublicationSweep.should_clear returns false.
    #[test]
    fn zone_sweep_no_clear_for_non_terminal_states() {
        use crate::types::LeaseState;
        for state in [
            LeaseState::Active,
            LeaseState::Suspended,
            LeaseState::Orphaned,
            LeaseState::Requested,
        ] {
            let sweep = ZonePublicationSweep::new(dummy_lease(), "ns", state);
            assert!(
                !sweep.should_clear(),
                "{state:?} must NOT trigger zone clear"
            );
        }
    }

    // ── Post-revocation zero-footprint contract ─────────────────────────────

    /// Zero footprint result passes assertion.
    #[test]
    fn zero_footprint_result_passes() {
        let result = CleanupResult {
            tiles_removed: 0,
            nodes_freed: 0,
            zone_publications_cleared: 0,
            zero_footprint: true,
        };
        assert!(result.assert_zero_footprint().is_ok());
    }

    /// Non-zero footprint result fails assertion.
    #[test]
    fn non_zero_footprint_result_fails() {
        let result = CleanupResult {
            tiles_removed: 2,
            nodes_freed: 5,
            zone_publications_cleared: 1,
            zero_footprint: false,
        };
        assert!(result.assert_zero_footprint().is_err());
    }

    // ── POST_REVOCATION_FREE_DELAY_MS constant ─────────────────────────────

    /// The free delay constant must be exactly 100 ms (spec line 254).
    #[test]
    fn post_revocation_free_delay_is_100ms() {
        assert_eq!(
            POST_REVOCATION_FREE_DELAY_MS, 100,
            "spec line 254 mandates 100ms delay"
        );
    }
}
