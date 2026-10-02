//! Lease lifecycle types: renewal policy, TTL tracking, orphan grace periods,
//! and post-revocation cleanup.
//!
//! The scene graph (`graph::leases`) owns live lease state; this module holds
//! the supporting types and pure helpers.

pub mod budget;
pub mod cleanup;
pub mod degradation;
pub mod orphan;
pub mod ttl;
pub mod types;

pub use types::LeaseId;
// LeaseState is defined in crate::types and re-exported here so that the
// lease module and all its sub-modules share one canonical definition.
pub use crate::types::LeaseState;
pub use budget::BudgetDelta;
pub use cleanup::{
    CleanupResult, POST_REVOCATION_FREE_DELAY_MS, PostRevocationCleanupSpec, RevocationKind,
    ZonePublicationSweep,
};
pub use orphan::{
    DEFAULT_GRACE_PERIOD_MS as ORPHAN_GRACE_PERIOD_MS, GRACE_PRECISION_MS, GracePeriodTimer,
    OrphanedLeaseSnapshot, TileVisualHint, ZonePublishResult, check_zone_publish_allowed,
};
pub use ttl::{AUTO_RENEW_THRESHOLD, AutoRenewalArm, DisarmReason, TtlCheck, TtlState};

// ─── Renewal Policy ──────────────────────────────────────────────────────────

/// Renewal policy for a lease.
///
/// From spec §Requirement: Auto-Renewal Policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenewalPolicy {
    /// Agent must explicitly renew before TTL expires.
    Manual,
    /// Runtime auto-renews at 75% TTL elapsed (when session Active, no budget violations).
    AutoRenew,
    /// Expires at TTL; no renewal option.
    OneShot,
}

// ─── Revoke Reason ───────────────────────────────────────────────────────────

/// Reason a lease was revoked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevokeReason {
    ViewerDismissed,
    BudgetPolicy,
    SuspensionTimeout,
    Other,
}

// ─── Resource Budget ─────────────────────────────────────────────────────────

// `ResourceBudget` is the single canonical type defined in `crate::types`.
// Re-exported here so that `budget.rs` and other lease sub-modules can import
// it via `super::ResourceBudget` without knowing the originating module.
pub use crate::types::ResourceBudget;
