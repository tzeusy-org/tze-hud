//! Lease lifecycle types: orphan grace period constants and the tile badge hint.
//!
//! The scene graph (`graph::leases`) owns live lease state; this module holds
//! the supporting types and pure helpers.

pub mod degradation;
pub mod orphan;
pub mod types;

pub use types::LeaseId;
// LeaseState is defined in crate::types and re-exported here so that the
// lease module and all its sub-modules share one canonical definition.
pub use crate::types::LeaseState;
pub use orphan::{DEFAULT_GRACE_PERIOD_MS as ORPHAN_GRACE_PERIOD_MS, TileVisualHint};

// Preserve the legacy import path for serialized scene budget metadata.
pub use crate::types::ResourceBudget;
