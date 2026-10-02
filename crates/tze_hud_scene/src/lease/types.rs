//! Lease identity types and auditable event types.
//!
//! Implements spec §Requirement: Lease Identity (lease-governance/spec.md lines 28-34)
//! and the auditable-event-per-transition requirement.

use crate::types::{Capability, ResourceBudget, SceneId};
use serde::{Deserialize, Serialize};

// ─── LeaseId ─────────────────────────────────────────────────────────────────

/// UUIDv7 lease identifier.  Time-ordered; assigned by the runtime at grant time.
///
/// Per spec: `LeaseId` is a `SceneId` type (UUIDv7).
pub type LeaseId = SceneId;

// ─── Deny Reason ─────────────────────────────────────────────────────────────

/// Reason a lease request was denied.
///
/// Populated in `LeaseResponse.deny_reason` on DENIED transitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyReason {
    /// Requested capabilities exceed what the session's grants allow.
    CapabilitiesExceeded,
    /// Requested priority exceeds the agent's capability ceiling.
    PriorityExceeded,
    /// Requested resource budget exceeds session maximum.
    BudgetExceeded,
    /// Runtime-wide lease limit (64) has been reached.
    MaxRuntimeLeasesExceeded,
    /// Per-session lease limit has been reached.
    MaxSessionLeasesExceeded,
    /// Safe mode is currently active; new lease requests are rejected.
    SafeModeActive,
    /// Agent session is not in Active state.
    SessionNotActive,
}

// ─── Revoke Reason ───────────────────────────────────────────────────────────

/// Reason a lease was revoked.  Populated in `LeaseResponse.revoke_reason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RevokeReason {
    /// Viewer dismissed a specific tile.
    ViewerDismissed,
    /// Budget policy enforcement: throttle sustained 30s or critical limit exceeded.
    BudgetPolicy,
    /// Lease remained SUSPENDED beyond `max_suspension_time_ms` (default 300,000ms).
    SuspensionTimeout,
    /// Agent's session capability was revoked by the runtime.
    CapabilityRevoked,
    /// Unspecified runtime-initiated revocation.
    Other,
}

// ─── Lease Identity ──────────────────────────────────────────────────────────

/// Full lease identity as required by spec §Requirement: Lease Identity.
///
/// Each granted lease carries all of these fields.  The wire protocol vends
/// these in `LeaseResponse` when `result = GRANTED`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeaseIdentity {
    /// UUIDv7 lease identifier (time-ordered).
    pub lease_id: LeaseId,
    /// Agent identity string, established at session auth.
    pub namespace: String,
    /// Parent session identifier.  Lease is invalidated if session is revoked.
    pub session_id: SceneId,
    /// Wall-clock grant timestamp in UTC microseconds (RFC 0003 wall-clock domain).
    pub granted_at_wall_us: u64,
    /// Lease duration in milliseconds.  0 = indefinite (subject to session TTL).
    pub ttl_ms: u64,
    /// Renewal policy.
    pub renewal_policy: crate::types::RenewalPolicy,
    /// Granted capability scope.
    pub capability_scope: Vec<Capability>,
    /// Resource budget granted at this lease.
    pub resource_budget: ResourceBudget,
    /// Effective lease priority (0=system, 1=high, 2=normal, 3=low, 4+=background).
    pub lease_priority: u8,
}

// ─── Auditable Lease Events ───────────────────────────────────────────────────

/// An auditable event emitted on every lease state transition.
///
/// Spec requirement: "Each transition produces an auditable event."
/// The wire conveys lease state through `LeaseResponse`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeaseAuditEvent {
    /// The lease this event applies to.
    pub lease_id: LeaseId,
    /// Wall-clock timestamp when this event was produced (microseconds).
    pub event_at_wall_us: u64,
    /// The kind of transition that occurred.
    pub kind: LeaseEventKind,
}

/// The kind of transition that produced a lease audit event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LeaseEventKind {
    /// REQUESTED → ACTIVE: lease granted successfully.
    Granted {
        identity: LeaseIdentity,
        expires_at_wall_us: Option<u64>,
    },
    /// REQUESTED → DENIED: lease request rejected.
    Denied { reason: DenyReason },
    /// ACTIVE → SUSPENDED: safe mode entered.
    Suspended {
        suspended_at_wall_us: u64,
        ttl_remaining_ms: u64,
    },
    /// SUSPENDED → ACTIVE: safe mode exited; TTL adjusted.
    Resumed {
        adjusted_expires_at_wall_us: Option<u64>,
        suspension_duration_us: u64,
    },
    /// ACTIVE → ORPHANED: agent session disconnected; grace period started.
    Orphaned { grace_expires_at_wall_us: u64 },
    /// ORPHANED → ACTIVE: agent reconnected within grace period.
    Reconnected,
    /// ACTIVE / ORPHANED → EXPIRED: TTL or grace period elapsed.
    Expired,
    /// → REVOKED: viewer dismiss, budget policy, or suspension timeout.
    Revoked { reason: RevokeReason },
    /// ACTIVE → RELEASED: agent voluntarily released the lease.
    Released,
    /// Capability removed from an active lease at runtime (live capability revocation).
    ///
    /// The lease remains ACTIVE; only the specified capability is removed from its scope.
    /// Future mutations requiring this capability will be rejected with `CapabilityMissing`.
    ///
    /// RFC 0001 §3.3: capability checks are enforced at mutation time against the live
    /// capability scope, not merely at grant time.
    CapabilityRevoked {
        /// The name of the capability that was removed.
        capability: String,
        /// Wall-clock timestamp when the revocation occurred (microseconds).
        revoked_at_wall_us: u64,
    },
}
