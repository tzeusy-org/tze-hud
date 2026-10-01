//! Per-agent budget accounting for shared resources.
//!
//! ## Authority Boundary
//!
//! This module is the **decoded-byte resource accounting authority**.
//!
//! - **`BudgetRegistry`** tracks how many decoded bytes each agent has consumed
//!   across all resource references. This is the single source of truth for
//!   `texture_bytes_per_tile` and `texture_bytes_total` budget checks.
//!
//! - **The enforcement ladder** (Warning → Throttle → Revoke) is NOT here.
//!   That lives in `tze_hud_runtime::budget::BudgetEnforcer`, which consumes
//!   budget check outcomes from this registry to advance its state machine.
//!
//! ## Budget semantics
//!
//! Texture bytes are charged to **the agent whose scene-graph node references
//! the resource**, not the uploader.  When multiple agents reference the same
//! resource each is charged the **full decoded size** against their respective
//! budgets (per-agent double-counting).  This prevents a coordinated
//! multi-agent budget bypass (spec lines 151–153).
//!
//! Budget is measured as **decoded in-memory size** (e.g., 4 MiB for a 1×1024²
//! RGBA8 image decoded from a 500 KiB compressed PNG), not the raw upload
//! size (spec lines 160–162).
//!
//! ## Mutation-time enforcement
//!
//! Budget checks for `texture_bytes_per_tile` and `texture_bytes_total` occur
//! in the mutation pipeline at per-mutation validation.  Checks are
//! all-or-nothing within a mutation batch (atomic pipeline) — if any
//! reference in the batch would exceed budget, the entire batch is rejected
//! with `BudgetExceeded` (spec lines 351–353, 356–358).
//!
//! ## Structure
//!
//! | Type | Purpose |
//! |---|---|
//! | `AgentResourceUsage` | Running decoded-bytes total per agent |
//! | `TileBudgetChecker` | Per-tile mutation-time budget validator |
//! | `BudgetRegistry` | Central per-agent registry |
//!
//! Source: RFC 0011 §4.3, §11.2, §11.3; spec lines 151–162, 351–358.

use std::collections::HashMap;

use crate::types::ResourceId;

// ─── Error types ──────────────────────────────────────────────────────────────

/// Budget violation reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetViolation {
    /// Per-tile texture budget exceeded.
    PerTileTextureBytes {
        tile_id: String,
        used_bytes: usize,
        limit_bytes: usize,
    },
    /// Per-agent total texture budget exceeded.
    AgentTotalTextureBytes {
        agent_ns: String,
        used_bytes: usize,
        limit_bytes: usize,
    },
}

impl std::fmt::Display for BudgetViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BudgetViolation::PerTileTextureBytes {
                tile_id,
                used_bytes,
                limit_bytes,
            } => write!(
                f,
                "per-tile budget exceeded for tile {tile_id}: {used_bytes} bytes > {limit_bytes} limit"
            ),
            BudgetViolation::AgentTotalTextureBytes {
                agent_ns,
                used_bytes,
                limit_bytes,
            } => write!(
                f,
                "agent total budget exceeded for {agent_ns}: {used_bytes} bytes > {limit_bytes} limit"
            ),
        }
    }
}

impl std::error::Error for BudgetViolation {}

// ─── Per-agent resource usage ─────────────────────────────────────────────────

/// Tracks decoded texture bytes used by a single agent across all its scene-
/// graph nodes.
///
/// Each (agent, resource) reference pair charges the full decoded size.
/// Multiple nodes in the same agent referencing the same resource each charge
/// that resource's decoded size.
#[derive(Debug, Default)]
pub struct AgentResourceUsage {
    /// Total decoded bytes currently charged to this agent.
    pub total_decoded_bytes: usize,
    /// Per-resource reference count within this agent's scene-graph nodes.
    /// `(ResourceId, node_ref_count)` — tracks how many nodes this agent has
    /// referencing each resource (to correctly charge/uncharge on deletion).
    resource_refs: HashMap<ResourceId, (usize, usize)>, // (ref_count, decoded_bytes_each)
}

impl AgentResourceUsage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Charge `decoded_bytes` for `resource_id` when a new node references it.
    ///
    /// Per spec: each node reference charges the full decoded size, regardless
    /// of how many other agents or nodes already reference the same resource.
    pub fn charge(&mut self, resource_id: ResourceId, decoded_bytes: usize) {
        let entry = self
            .resource_refs
            .entry(resource_id)
            .or_insert((0, decoded_bytes));
        // Decoded size is immutable per resource; assert consistency on re-charge.
        debug_assert_eq!(
            entry.1, decoded_bytes,
            "inconsistent decoded_bytes for ResourceId when charging usage"
        );
        entry.0 += 1;
        self.total_decoded_bytes += entry.1;
    }

    /// Uncharge `decoded_bytes` for `resource_id` when a node is deleted.
    ///
    /// Silently no-ops if the resource is not in this agent's usage table
    /// (defensive: prevents underflow panics on cleanup paths).
    pub fn uncharge(&mut self, resource_id: &ResourceId) {
        if let Some((ref_count, decoded_bytes)) = self.resource_refs.get_mut(resource_id)
            && *ref_count > 0
        {
            *ref_count -= 1;
            let charged = *decoded_bytes;
            if *ref_count == 0 {
                self.resource_refs.remove(resource_id);
            }
            self.total_decoded_bytes = self.total_decoded_bytes.saturating_sub(charged);
        }
    }

    /// Current total decoded bytes charged to this agent.
    pub fn total_bytes(&self) -> usize {
        self.total_decoded_bytes
    }

    /// Number of distinct resources referenced by this agent.
    pub fn distinct_resource_count(&self) -> usize {
        self.resource_refs.len()
    }
}

// ─── Budget registry ──────────────────────────────────────────────────────────

/// Central per-agent budget registry.
///
/// Maintains `AgentResourceUsage` entries per agent namespace and enforces
/// limits during mutation-time validation.
#[derive(Debug, Default)]
pub struct BudgetRegistry {
    agents: HashMap<String, AgentResourceUsage>,
}

impl BudgetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `agent_ns` now has a scene-graph node referencing
    /// `resource_id` with `decoded_bytes` in-memory size.
    ///
    /// This is called during mutation commit, after the scene mutation is
    /// accepted.
    pub fn on_node_ref_added(
        &mut self,
        agent_ns: &str,
        resource_id: ResourceId,
        decoded_bytes: usize,
    ) {
        self.agents
            .entry(agent_ns.to_owned())
            .or_default()
            .charge(resource_id, decoded_bytes);
    }

    /// Record that `agent_ns` deleted a scene-graph node that referenced
    /// `resource_id`.
    ///
    /// Called during mutation commit when a node deletion cascades.
    pub fn on_node_ref_removed(&mut self, agent_ns: &str, resource_id: &ResourceId) {
        if let Some(usage) = self.agents.get_mut(agent_ns) {
            usage.uncharge(resource_id);
        }
    }

    /// Current decoded bytes used by `agent_ns`.
    pub fn agent_used_bytes(&self, agent_ns: &str) -> usize {
        self.agents
            .get(agent_ns)
            .map(|u| u.total_bytes())
            .unwrap_or(0)
    }

    /// Check whether adding `new_decoded_bytes` for `agent_ns` would breach
    /// `agent_limit_bytes`.  If `agent_limit_bytes` is 0, the check is
    /// skipped (unlimited).
    pub fn check_agent_limit(
        &self,
        agent_ns: &str,
        new_decoded_bytes: usize,
        agent_limit_bytes: usize,
    ) -> Result<(), BudgetViolation> {
        if agent_limit_bytes == 0 {
            return Ok(()); // 0 = unlimited
        }
        let current = self.agent_used_bytes(agent_ns);
        let projected = current.saturating_add(new_decoded_bytes);
        if projected > agent_limit_bytes {
            Err(BudgetViolation::AgentTotalTextureBytes {
                agent_ns: agent_ns.to_owned(),
                used_bytes: projected,
                limit_bytes: agent_limit_bytes,
            })
        } else {
            Ok(())
        }
    }

    /// Remove all usage records for `agent_ns`.
    ///
    /// Called after agent revocation; leaves the agent's resources with
    /// refcount 0 (GC-eligible) but removes budget accounting.
    pub fn remove_agent(&mut self, agent_ns: &str) {
        self.agents.remove(agent_ns);
    }
}

// ─── Per-tile budget checker ──────────────────────────────────────────────────

/// Validates a mutation batch against per-tile and per-agent texture budgets.
///
/// The checker is instantiated per mutation batch and accumulates the decoded
/// bytes that would be added.  `validate()` returns the first violation (if
/// any).  The check is all-or-nothing: if any resource addition fails, the
/// entire batch must be rejected (spec lines 351–353).
///
/// ## Usage
///
/// ```ignore
/// let mut checker = TileBudgetChecker::new("tile-123", 8 * 1024 * 1024);
/// for (resource_id, decoded_bytes) in new_refs {
///     checker.add_ref(resource_id, decoded_bytes);
/// }
/// checker.validate()?; // returns Err if over budget
/// // Only commit if validate() returns Ok.
/// ```
pub struct TileBudgetChecker {
    tile_id: String,
    /// Current accumulated decoded bytes for this tile across all existing nodes.
    current_tile_bytes: usize,
    /// Maximum decoded bytes allowed per tile.
    per_tile_limit_bytes: usize,
    /// Additional bytes this mutation batch would add.
    batch_added_bytes: usize,
}

impl TileBudgetChecker {
    /// Create a checker for a tile.
    ///
    /// `current_tile_bytes`: total decoded bytes of resources currently
    /// referenced by nodes in this tile (before this mutation batch).
    ///
    /// `per_tile_limit_bytes`: 0 means unlimited.
    pub fn new(
        tile_id: impl Into<String>,
        current_tile_bytes: usize,
        per_tile_limit_bytes: usize,
    ) -> Self {
        Self {
            tile_id: tile_id.into(),
            current_tile_bytes,
            per_tile_limit_bytes,
            batch_added_bytes: 0,
        }
    }

    /// Register that this mutation batch adds a node referencing `decoded_bytes`.
    pub fn add_ref(&mut self, _resource_id: ResourceId, decoded_bytes: usize) {
        self.batch_added_bytes = self.batch_added_bytes.saturating_add(decoded_bytes);
    }

    /// Validate the accumulated batch against the per-tile limit.
    ///
    /// Returns `Err(BudgetViolation::PerTileTextureBytes)` if the batch
    /// would push the tile over its limit.
    pub fn validate(&self) -> Result<(), BudgetViolation> {
        if self.per_tile_limit_bytes == 0 {
            return Ok(()); // unlimited
        }
        let projected = match self.current_tile_bytes.checked_add(self.batch_added_bytes) {
            Some(sum) => sum,
            None => {
                // Treat arithmetic overflow as exceeding the budget.
                return Err(BudgetViolation::PerTileTextureBytes {
                    tile_id: self.tile_id.clone(),
                    used_bytes: usize::MAX,
                    limit_bytes: self.per_tile_limit_bytes,
                });
            }
        };
        if projected > self.per_tile_limit_bytes {
            Err(BudgetViolation::PerTileTextureBytes {
                tile_id: self.tile_id.clone(),
                used_bytes: projected,
                limit_bytes: self.per_tile_limit_bytes,
            })
        } else {
            Ok(())
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ResourceId;

    fn id(n: u8) -> ResourceId {
        ResourceId::from_content(&[n])
    }

    // Acceptance: Agent A (10 MiB) and B (10 MiB) both ref 4 MiB → each charged 4 MiB
    // [spec line 157-158].
    #[test]
    fn double_counting_for_shared_resource() {
        let mut registry = BudgetRegistry::new();
        let resource = id(0x01);
        let decoded = 4 * 1024 * 1024; // 4 MiB

        registry.on_node_ref_added("agent_a", resource, decoded);
        registry.on_node_ref_added("agent_b", resource, decoded);

        assert_eq!(
            registry.agent_used_bytes("agent_a"),
            decoded,
            "A charged 4 MiB"
        );
        assert_eq!(
            registry.agent_used_bytes("agent_b"),
            decoded,
            "B charged 4 MiB"
        );
    }

    // Acceptance: decoded size charged, not compressed [spec line 161-162].
    #[test]
    fn decoded_size_not_compressed_size_charged() {
        let mut registry = BudgetRegistry::new();
        let resource = id(0x02);
        let compressed = 512 * 1024; // 500 KiB compressed PNG
        let decoded = 4 * 1024 * 1024; // 4 MiB RGBA8

        // Only the decoded size should be passed in; the caller is responsible
        // for using decoded_bytes, not raw bytes.
        registry.on_node_ref_added("agent_a", resource, decoded);

        // Ensure we would NOT accept compressed size as the budget value.
        assert_ne!(registry.agent_used_bytes("agent_a"), compressed);
        assert_eq!(registry.agent_used_bytes("agent_a"), decoded);
    }

    // Acceptance: node deletion unchargs the agent.
    #[test]
    fn uncharge_on_node_deletion() {
        let mut registry = BudgetRegistry::new();
        let resource = id(0x03);
        let decoded = 1024 * 1024;

        registry.on_node_ref_added("agent_a", resource, decoded);
        assert_eq!(registry.agent_used_bytes("agent_a"), decoded);

        registry.on_node_ref_removed("agent_a", &resource);
        assert_eq!(registry.agent_used_bytes("agent_a"), 0);
    }

    // Acceptance: multiple nodes in same agent referencing same resource each charge full size.
    #[test]
    fn multiple_nodes_same_resource_same_agent() {
        let mut registry = BudgetRegistry::new();
        let resource = id(0x04);
        let decoded = 1024 * 1024;

        registry.on_node_ref_added("agent_a", resource, decoded);
        registry.on_node_ref_added("agent_a", resource, decoded);

        assert_eq!(registry.agent_used_bytes("agent_a"), decoded * 2);

        // Delete one node.
        registry.on_node_ref_removed("agent_a", &resource);
        assert_eq!(registry.agent_used_bytes("agent_a"), decoded);

        // Delete last.
        registry.on_node_ref_removed("agent_a", &resource);
        assert_eq!(registry.agent_used_bytes("agent_a"), 0);
    }

    // Acceptance: check_agent_limit returns Ok when under limit.
    #[test]
    fn check_agent_limit_within_budget() {
        let registry = BudgetRegistry::new();
        let limit = 10 * 1024 * 1024; // 10 MiB
        let result = registry.check_agent_limit("agent_a", 4 * 1024 * 1024, limit);
        assert!(result.is_ok());
    }

    // Acceptance: check_agent_limit returns Err when over limit.
    #[test]
    fn check_agent_limit_exceeds_budget() {
        let mut registry = BudgetRegistry::new();
        let resource = id(0x05);
        let limit = 10 * 1024 * 1024; // 10 MiB

        // Agent already uses 8 MiB.
        registry.on_node_ref_added("agent_a", resource, 8 * 1024 * 1024);

        // Adding 3 MiB more would exceed 10 MiB.
        let result = registry.check_agent_limit("agent_a", 3 * 1024 * 1024, limit);
        assert!(matches!(
            result,
            Err(BudgetViolation::AgentTotalTextureBytes { .. })
        ));
    }

    // Acceptance: per-tile budget check — batch adding too much rejected.
    #[test]
    fn tile_budget_exceeded_rejects_batch() {
        let limit = 8 * 1024 * 1024; // 8 MiB per tile
        let mut checker = TileBudgetChecker::new("tile-1", 0, limit);

        // Add refs totalling 9 MiB.
        checker.add_ref(id(0x10), 5 * 1024 * 1024);
        checker.add_ref(id(0x11), 4 * 1024 * 1024);

        let result = checker.validate();
        assert!(matches!(
            result,
            Err(BudgetViolation::PerTileTextureBytes { .. })
        ));
    }

    // Acceptance: per-tile budget check — batch within limit accepted.
    #[test]
    fn tile_budget_within_limit_accepted() {
        let limit = 8 * 1024 * 1024;
        let mut checker = TileBudgetChecker::new("tile-2", 0, limit);

        checker.add_ref(id(0x12), 3 * 1024 * 1024);
        checker.add_ref(id(0x13), 4 * 1024 * 1024);

        assert!(checker.validate().is_ok());
    }

    // Acceptance: 0 limit means unlimited.
    #[test]
    fn zero_limit_means_unlimited() {
        let registry = BudgetRegistry::new();
        // Even a huge addition should be Ok.
        let result = registry.check_agent_limit("agent_a", usize::MAX / 2, 0);
        assert!(result.is_ok());
    }

    // Acceptance: remove_agent clears all usage.
    #[test]
    fn remove_agent_clears_budget() {
        let mut registry = BudgetRegistry::new();
        registry.on_node_ref_added("agent_a", id(0x20), 1024 * 1024);
        assert_eq!(registry.agent_used_bytes("agent_a"), 1024 * 1024);

        registry.remove_agent("agent_a");
        assert_eq!(registry.agent_used_bytes("agent_a"), 0);
    }
}
