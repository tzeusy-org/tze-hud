//! Lease lifecycle types: orphan grace period constants and the tile badge hint.
//!
//! The scene graph (`graph::leases`) owns live lease state; this module holds
//! the supporting types and pure helpers.

pub mod budget;
pub mod degradation;
pub mod orphan;
pub mod types;

pub use types::LeaseId;
// LeaseState is defined in crate::types and re-exported here so that the
// lease module and all its sub-modules share one canonical definition.
pub use crate::types::LeaseState;
pub use budget::BudgetDelta;
pub use orphan::{DEFAULT_GRACE_PERIOD_MS as ORPHAN_GRACE_PERIOD_MS, TileVisualHint};

// ─── Resource Budget ─────────────────────────────────────────────────────────

// `ResourceBudget` is the single canonical type defined in `crate::types`.
// Re-exported here so that `budget.rs` and other lease sub-modules can import
// it via `super::ResourceBudget` without knowing the originating module.
pub use crate::types::ResourceBudget;
