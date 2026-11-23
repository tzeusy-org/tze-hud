//! Validation errors for scene graph operations.
//!
use crate::types::{ResourceId, SceneId};
use thiserror::Error;

// ─── Low-level ValidationError (internal per-operation) ──────────────────────

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ValidationError {
    #[error("tab not found: {id}")]
    TabNotFound { id: SceneId },

    #[error("tile not found: {id}")]
    TileNotFound { id: SceneId },

    #[error("node not found: {id}")]
    NodeNotFound { id: SceneId },

    #[error("invalid portal surface for tile {tile_id}: {reason}")]
    InvalidPortalSurface { tile_id: SceneId, reason: String },

    #[error("portal surface not found for tile: {tile_id}")]
    PortalSurfaceNotFound { tile_id: SceneId },

    #[error("lease not found: {id}")]
    LeaseNotFound { id: SceneId },

    #[error("lease expired: {id}")]
    LeaseExpired { id: SceneId },

    #[error("duplicate display order: {order}")]
    DuplicateDisplayOrder { order: u32 },

    /// An ID that must be unique appears more than once in the scene graph.
    /// no TabId, TileId, or NodeId may appear more than once.
    #[error("duplicate id: {id}")]
    DuplicateId { id: SceneId },

    #[error("invalid field '{field}': {reason}")]
    InvalidField { field: String, reason: String },

    /// Tile bounds failed a spatial constraint (non-positive dimensions or out-of-display-area).
    #[error("bounds out of range: {reason}")]
    BoundsOutOfRange { reason: String },

    #[error("budget exceeded: {resource}")]
    BudgetExceeded { resource: String },

    /// A tile already has the maximum number of nodes (64 ).
    #[error("node count exceeded for tile {tile_id}: has {current}, limit {limit}")]
    NodeCountExceeded {
        tile_id: SceneId,
        current: usize,
        limit: usize,
    },

    /// A mutation referenced a ResourceId that is not registered with the runtime.
    /// StaticImageNode referencing unknown ResourceId.
    #[error("resource not found: {id}")]
    ResourceNotFound { id: ResourceId },

    #[error("zone not found: {name}")]
    ZoneNotFound { name: String },

    #[error("zone media type mismatch for zone '{zone}'")]
    ZoneMediaTypeMismatch { zone: String },

    #[error("zone '{zone}' has reached max publishers ({max})")]
    ZoneMaxPublishersReached { zone: String, max: u32 },

    #[error("zone '{zone}' has reached max keys ({max})")]
    ZoneMaxKeysReached { zone: String, max: u32 },

    /// A mutation was rejected because the requesting agent does not own the target tile.
    /// namespace isolation.
    #[error(
        "namespace mismatch: tile {tile_id} belongs to namespace '{tile_namespace}', not '{agent_namespace}'"
    )]
    NamespaceMismatch {
        tile_id: SceneId,
        tile_namespace: String,
        agent_namespace: String,
    },

    /// Two non-passthrough tiles on the same tab share a z_order with overlapping bounds.
    #[error(
        "z-order conflict: tiles {tile_a} and {tile_b} share z_order {z_order} with overlapping bounds"
    )]
    ZOrderConflict {
        tile_a: SceneId,
        tile_b: SceneId,
        z_order: u32,
    },

    // ── Lease-state zone-publish errors ──
    /// Zone publish rejected: no active lease found for publisher namespace.
    ///
    ///: "Missing lease MUST produce `LEASE_NOT_FOUND`".
    #[error("zone publish rejected: lease not found for namespace '{namespace}'")]
    ZonePublishLeaseNotFound { namespace: String },

    /// Zone publish rejected: lease exists but is not in ACTIVE state.
    ///
    ///: "inactive lease `LEASE_NOT_ACTIVE`".
    /// Covers other non-ACTIVE lease states not handled by specialized variants
    /// (for example, terminal states such as REVOKED, EXPIRED, RELEASED, DENIED).
    /// SUSPENDED and ORPHANED have dedicated variants (`ZonePublishSafeModeActive`,
    /// `ZonePublishLeaseOrphaned`).
    #[error("zone publish rejected: lease for '{namespace}' is not active (state: {state})")]
    ZonePublishLeaseNotActive { namespace: String, state: String },

    /// Zone publish rejected: lease is SUSPENDED (safe mode active).
    ///
    ///: "New zone publishes under the suspended lease MUST be
    /// rejected with `SAFE_MODE_ACTIVE`".
    #[error("zone publish rejected: safe mode active for lease '{namespace}'")]
    ZonePublishSafeModeActive { namespace: String },

    /// Zone publish rejected: lease is ORPHANED (agent disconnected).
    ///
    /// Existing publications remain
    /// visible with stale badge; new publishes are rejected.
    #[error("zone publish rejected: lease for '{namespace}' is orphaned (agent disconnected)")]
    ZonePublishLeaseOrphaned { namespace: String },

    // ── Widget publish errors ─────────────────────────────
    /// Widget publish rejected: widget instance not found in the registry.
    #[error("widget not found: '{name}'")]
    WidgetNotFound { name: String },

    /// Widget publish rejected: parameter name not declared in the widget's schema.
    #[error("widget unknown parameter '{param}' in widget '{widget}'")]
    WidgetUnknownParameter { widget: String, param: String },

    /// Widget publish rejected: published value type does not match schema type.
    #[error("{widget}.{param} must be {expected}")]
    WidgetParameterTypeMismatch {
        widget: String,
        param: String,
        expected: String,
    },

    /// Widget publish rejected: value is invalid (NaN, infinity, out-of-range string, bad enum).
    #[error("widget parameter invalid value for '{param}' in widget '{widget}': {reason}")]
    WidgetParameterInvalidValue {
        widget: String,
        param: String,
        reason: String,
    },

    /// Widget publish rejected: publisher has reached the per-namespace publication limit.
    ///
    /// Mirrors `ZoneMaxPublishersReached` for the widget publish path.
    /// Returned by `publish_to_widget` under `Stack` contention when the
    /// publisher's active record count equals `WidgetDefinition::max_publishers`.
    #[error("widget '{widget}' has reached max publishers ({max}) for namespace")]
    WidgetMaxPublishersReached { widget: String, max: u32 },
}
