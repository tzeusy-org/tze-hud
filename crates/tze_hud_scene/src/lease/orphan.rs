//! Orphan-window constants and the tile badge hint.
//!
//! A disconnected session's leases are orphaned for a reconnect grace window
//! (`DEFAULT_GRACE_PERIOD_MS`); the scene graph (`graph::leases`) drives the
//! transitions and sets `TileVisualHint::DisconnectionBadge` on frozen tiles.

// ─── Constants ───────────────────────────────────────────────────────────────

/// Reconnect grace period (ms). Spec: "default 30,000 ms" (line 133).
pub const DEFAULT_GRACE_PERIOD_MS: u64 = 30_000;

// ─── TileVisualHint ──────────────────────────────────────────────────────────

/// Visual overlay hint to display on a tile's rendered surface.
///
/// The compositor renders these within one frame of a state change.
/// Spec requirements:
/// - `DisconnectionBadge`: "Disconnection badge MUST appear within 1 frame"
///   (line 133).
/// - `StaleBadge`: zone publications during ORPHANED state are "stale-badged"
///   (lines 231–233, adapted).
/// - `BudgetWarning`: amber border for budget ≥ 80% (line 170).
/// - `None`: normal rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum TileVisualHint {
    /// Tile renders normally.
    #[default]
    None,
    /// Agent is disconnected; tile is frozen at last state.
    /// Displayed within 1 frame of `disconnect` (spec line 133).
    DisconnectionBadge,
    /// Tile content is stale because the controlling lease is ORPHANED;
    /// new publishes to this tile's zone are rejected.
    StaleBadge,
    /// Budget soft limit (≥ 80%) reached; amber border.
    BudgetWarning,
}
