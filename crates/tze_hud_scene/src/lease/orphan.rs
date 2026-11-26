//! Orphan-window constants and the tile badge hint.
//!
//! A disconnected session's leases are orphaned for a reconnect grace window
//! (`DEFAULT_GRACE_PERIOD_MS`); the scene graph (`graph::leases`) drives the
//! transitions and sets `TileVisualHint::DisconnectionBadge` on frozen tiles.

// ─── Constants ───────────────────────────────────────────────────────────────

/// Default reconnect grace period: 30,000 ms.
pub const DEFAULT_GRACE_PERIOD_MS: u64 = 30_000;

// ─── TileVisualHint ──────────────────────────────────────────────────────────

/// Visual overlay hint to display on a tile's rendered surface.
///
/// The compositor renders lifecycle hints within one frame of a state change.
/// - `DisconnectionBadge`: appears within one frame of the disconnect.
/// - `StaleBadge`: zone publications during the ORPHANED state.
/// - `BudgetWarning`: legacy serialized value; no current accounting producer.
/// - `None`: normal rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum TileVisualHint {
    /// Tile renders normally.
    #[default]
    None,
    /// Agent is disconnected; tile is frozen at last state.
    /// Displayed within 1 frame of `disconnect`.
    DisconnectionBadge,
    /// Tile content is stale because the controlling lease is ORPHANED;
    /// new publishes to this tile's zone are rejected.
    StaleBadge,
    /// Legacy budget warning retained for serialized scene compatibility.
    BudgetWarning,
}
