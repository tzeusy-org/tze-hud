//! Resource delta proposed by a mutation batch, checked by the runtime budget gate.

/// The resource delta proposed by a single `MutationBatch`.
///
/// Negative values indicate resource releases (e.g. `DeleteTile`).
#[derive(Clone, Copy, Debug, Default)]
pub struct BudgetDelta {
    /// Tiles to be created (positive) or deleted (negative) by the batch.
    pub delta_tiles: i32,
    /// Maximum nodes-per-tile in any tile touched by this batch.
    pub max_nodes_in_batch: u32,
    /// Texture bytes added (positive) or released (negative).
    pub delta_texture_bytes: i64,
}
