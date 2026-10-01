//! Lease priority assignment and sort semantics.
//!
//! Implements:
//! - Requirement: Priority Assignment (lease-governance/spec.md lines 49-60)
//! - Requirement: Priority Sort Semantics (lease-governance/spec.md lines 62-69)
//!
//! ## Priority values
//! | Value | Meaning |
//! |-------|---------|
//! | 0     | System / chrome — reserved; agents MUST NOT request this |
//! | 1     | High priority — requires `lease:priority:1` capability |
//! | 2     | Normal (default) |
//! | 3     | Low |
//! | 4+    | Background |
//!
//! Numerically lower value = higher rendering priority (0 is highest).

use crate::types::Capability;

// ─── Constants ────────────────────────────────────────────────────────────────

/// Priority reserved for system / chrome (runtime-internal only).
pub const PRIORITY_SYSTEM: u8 = 0;
/// Priority requiring `lease:priority:1` capability.
pub const PRIORITY_HIGH: u8 = 1;
/// Default priority granted to agents (spec line 50, "Priority 2 MUST be the default").
pub const PRIORITY_DEFAULT: u8 = 2;

// ─── Priority Assignment ─────────────────────────────────────────────────────

/// Clamp a requested priority according to the spec rules:
///
/// - Priority 0 → downgraded to 2 (system-reserved).
/// - Priority 1 → downgraded to 2 unless `capabilities` contains `Capability::LeasePriority1`.
/// - All other values → passed through as-is.
///
/// From spec §Requirement: Priority Assignment (lines 49-60):
/// > "An agent requesting priority 0 MUST receive priority 2.
/// >  An agent requesting priority 1 without the capability MUST receive priority 2."
pub fn clamp_requested_priority(requested: u8, capabilities: &[Capability]) -> u8 {
    match requested {
        PRIORITY_SYSTEM => {
            // Priority 0 is reserved for system/chrome — always downgrade.
            PRIORITY_DEFAULT
        }
        PRIORITY_HIGH => {
            // Priority 1 requires the lease:priority:1 capability.
            if capabilities.contains(&Capability::LeasePriority1) {
                PRIORITY_HIGH
            } else {
                PRIORITY_DEFAULT
            }
        }
        other => other,
    }
}

// ─── Sort Key ─────────────────────────────────────────────────────────────────

/// The compositor sort key for tiles: `(lease_priority ASC, z_order DESC)`.
///
/// Lower numeric `lease_priority` = higher rendering priority.
/// Within the same priority class, higher `z_order` wins.
///
/// From spec §Requirement: Priority Sort Semantics (lines 62-69).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileSortKey {
    /// Lease priority (lower = higher priority — renders on top).
    pub lease_priority: u8,
    /// Z-order within the priority class (higher = on top of peers with same priority).
    pub z_order: u32,
}

impl TileSortKey {
    /// Construct a sort key from lease priority and tile z-order.
    pub fn new(lease_priority: u8, z_order: u32) -> Self {
        TileSortKey {
            lease_priority,
            z_order,
        }
    }

    /// Returns `true` if `self` should render *above* `other`.
    ///
    /// A tile renders above another if its `lease_priority` is numerically lower,
    /// or, when priorities are equal, if its `z_order` is higher.
    pub fn renders_above(&self, other: &TileSortKey) -> bool {
        match self.lease_priority.cmp(&other.lease_priority) {
            std::cmp::Ordering::Less => true, // lower priority number = higher rendering priority
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => self.z_order > other.z_order,
        }
    }
}

/// Ordering for use with `sort_unstable_by_key`.
///
/// Sorts tiles from *highest* rendering priority to *lowest*, i.e. the tile that
/// should appear on top comes first.
///
/// Sort key: `(lease_priority ASC, z_order DESC)` per spec lines 62-69.
///
/// To get the natural ascending-first order for shed selection (least important
/// first), reverse the result or call `shed_order_key` instead.
impl PartialOrd for TileSortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TileSortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Primary: lease_priority ASC (lower priority number = renders above = sort earlier)
        // Secondary: z_order DESC (higher z_order = renders above within same priority)
        match self.lease_priority.cmp(&other.lease_priority) {
            std::cmp::Ordering::Equal => other.z_order.cmp(&self.z_order), // DESC
            ord => ord,                                                    // ASC
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Capability;

    // ── Priority clamping ────────────────────────────────────────────────────

    /// WHEN an agent requests priority 0 THEN it receives priority 2.
    #[test]
    fn priority_0_downgraded_to_2() {
        assert_eq!(clamp_requested_priority(0, &[]), PRIORITY_DEFAULT);
        // Even with the high-priority capability, 0 is always downgraded.
        assert_eq!(
            clamp_requested_priority(0, &[Capability::LeasePriority1]),
            PRIORITY_DEFAULT
        );
    }

    /// WHEN an agent requests priority 1 without `lease:priority:1` THEN it receives priority 2.
    #[test]
    fn priority_1_without_capability_downgraded_to_2() {
        assert_eq!(clamp_requested_priority(1, &[]), PRIORITY_DEFAULT);
        assert_eq!(
            clamp_requested_priority(1, &[Capability::CreateTiles]),
            PRIORITY_DEFAULT
        );
    }

    /// WHEN an agent requests priority 1 WITH `lease:priority:1` THEN it receives priority 1.
    #[test]
    fn priority_1_with_capability_granted() {
        assert_eq!(
            clamp_requested_priority(1, &[Capability::LeasePriority1]),
            PRIORITY_HIGH
        );
    }

    /// WHEN an agent requests priority 2 THEN it receives priority 2 (no change).
    #[test]
    fn priority_2_passes_through() {
        assert_eq!(clamp_requested_priority(2, &[]), PRIORITY_DEFAULT);
    }

    /// WHEN an agent requests priority 3 THEN it receives priority 3 (no change).
    #[test]
    fn priority_3_passes_through() {
        assert_eq!(clamp_requested_priority(3, &[]), 3u8);
    }

    // ── Sort key ordering ────────────────────────────────────────────────────

    /// Lower lease_priority renders above higher lease_priority.
    #[test]
    fn sort_key_lower_priority_renders_above() {
        let high = TileSortKey::new(1, 5);
        let normal = TileSortKey::new(2, 5);
        assert!(high.renders_above(&normal));
        assert!(!normal.renders_above(&high));
    }

    /// Same priority: higher z_order renders above.
    #[test]
    fn sort_key_same_priority_higher_z_wins() {
        let top = TileSortKey::new(2, 10);
        let bottom = TileSortKey::new(2, 5);
        assert!(top.renders_above(&bottom));
        assert!(!bottom.renders_above(&top));
    }

    /// Ord: keys sort ascending by (lease_priority ASC, z_order DESC).
    #[test]
    fn sort_key_ord_ascending() {
        let mut keys = [
            TileSortKey::new(3, 1),  // least important
            TileSortKey::new(1, 10), // most important
            TileSortKey::new(2, 5),  // middle
        ];
        keys.sort();
        assert_eq!(keys[0], TileSortKey::new(1, 10)); // highest-priority tile first
        assert_eq!(keys[2], TileSortKey::new(3, 1)); // lowest-priority tile last
    }
}
