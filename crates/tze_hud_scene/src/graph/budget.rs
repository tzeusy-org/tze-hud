use super::*;

impl SceneGraph {
    // ─── Budget enforcement ─────────────────────────────────────────────

    /// Get current resource usage for a lease.
    pub fn lease_resource_usage(&self, lease_id: &SceneId) -> ResourceUsage {
        let mut usage = ResourceUsage::default();
        for tile in self.tiles.values().filter(|t| t.lease_id == *lease_id) {
            usage.tiles += 1;
            // Count nodes in this tile
            let node_count = self.count_nodes_in_tile(tile);
            usage.nodes_per_tile.insert(tile.id, node_count);
            // Sum texture bytes for static image nodes in this tile
            if let Some(root_id) = tile.root_node {
                usage.texture_bytes += self.sum_texture_bytes(root_id);
            }
        }
        usage
    }

    /// Compute the logical lease-usage delta represented by a batch.
    ///
    /// This is the transport/runtime admission input. It deliberately counts
    /// shared logical references per lease; physical resident allocation is a
    /// separate resource-ledger domain.
    pub fn mutation_budget_delta(
        &self,
        lease_id: &SceneId,
        batch: &crate::mutation::MutationBatch,
    ) -> crate::lease::BudgetDelta {
        let mut delta_tiles = 0_i32;
        let mut delta_texture_bytes = 0_i64;
        let mut max_nodes_in_batch = 0_u32;
        let mut projected_tile_texture_bytes = std::collections::HashMap::<SceneId, u64>::new();
        let mut projected_node_texture_bytes = std::collections::HashMap::<SceneId, u64>::new();
        for mutation in &batch.mutations {
            match mutation {
                crate::mutation::SceneMutation::CreateTile { .. } => {
                    delta_tiles = delta_tiles.saturating_add(1);
                }
                crate::mutation::SceneMutation::DeleteTile { tile_id } => {
                    if let Some(tile) = self.tiles.get(tile_id)
                        && tile.lease_id == *lease_id
                    {
                        delta_tiles = delta_tiles.saturating_sub(1);
                        let old_bytes = projected_tile_texture_bytes
                            .remove(tile_id)
                            .unwrap_or_else(|| {
                                tile.root_node
                                    .map(|root| self.sum_texture_bytes(root))
                                    .unwrap_or(0)
                            });
                        delta_texture_bytes = delta_texture_bytes
                            .saturating_sub(i64::try_from(old_bytes).unwrap_or(i64::MAX));
                    }
                }
                crate::mutation::SceneMutation::SetTileRoot {
                    tile_id,
                    node,
                    descendants,
                } => {
                    max_nodes_in_batch = max_nodes_in_batch.max(
                        u32::try_from(descendants.len().saturating_add(1)).unwrap_or(u32::MAX),
                    );
                    let old_bytes = projected_tile_texture_bytes
                        .get(tile_id)
                        .copied()
                        .unwrap_or_else(|| {
                            self.tiles
                                .get(tile_id)
                                .and_then(|tile| tile.root_node)
                                .map(|root| self.sum_texture_bytes(root))
                                .unwrap_or(0)
                        });
                    let new_bytes = Self::count_texture_bytes_in_node(node).saturating_add(
                        descendants
                            .iter()
                            .map(Self::count_texture_bytes_in_node)
                            .sum(),
                    );
                    delta_texture_bytes = delta_texture_bytes.saturating_add(
                        i64::try_from(new_bytes)
                            .unwrap_or(i64::MAX)
                            .saturating_sub(i64::try_from(old_bytes).unwrap_or(i64::MAX)),
                    );
                    projected_tile_texture_bytes.insert(*tile_id, new_bytes);
                    projected_node_texture_bytes
                        .insert(node.id, Self::count_texture_bytes_in_node(node));
                    for descendant in descendants {
                        projected_node_texture_bytes
                            .insert(descendant.id, Self::count_texture_bytes_in_node(descendant));
                    }
                }
                crate::mutation::SceneMutation::AddNode { tile_id, node, .. } => {
                    max_nodes_in_batch = max_nodes_in_batch.max(1);
                    let added = Self::count_texture_bytes_in_node(node);
                    delta_texture_bytes = delta_texture_bytes
                        .saturating_add(i64::try_from(added).unwrap_or(i64::MAX));
                    let tile_bytes =
                        projected_tile_texture_bytes
                            .entry(*tile_id)
                            .or_insert_with(|| {
                                self.tiles
                                    .get(tile_id)
                                    .and_then(|tile| tile.root_node)
                                    .map(|root| self.sum_texture_bytes(root))
                                    .unwrap_or(0)
                            });
                    *tile_bytes = tile_bytes.saturating_add(added);
                    projected_node_texture_bytes.insert(node.id, added);
                }
                crate::mutation::SceneMutation::UpdateNodeContent {
                    tile_id,
                    node_id,
                    data: NodeData::StaticImage(new_image),
                    ..
                } => {
                    let old_bytes = projected_node_texture_bytes
                        .get(node_id)
                        .copied()
                        .unwrap_or_else(|| {
                            self.nodes
                                .get(node_id)
                                .and_then(|node| match &node.data {
                                    NodeData::StaticImage(image) => Some(image.decoded_bytes),
                                    _ => None,
                                })
                                .unwrap_or(0)
                        });
                    let new_bytes = if new_image.decoded_bytes == 0 {
                        old_bytes
                    } else {
                        new_image.decoded_bytes
                    };
                    let change = i64::try_from(new_bytes)
                        .unwrap_or(i64::MAX)
                        .saturating_sub(i64::try_from(old_bytes).unwrap_or(i64::MAX));
                    delta_texture_bytes = delta_texture_bytes.saturating_add(change);
                    projected_node_texture_bytes.insert(*node_id, new_bytes);
                    let tile_bytes =
                        projected_tile_texture_bytes
                            .entry(*tile_id)
                            .or_insert_with(|| {
                                self.tiles
                                    .get(tile_id)
                                    .and_then(|tile| tile.root_node)
                                    .map(|root| self.sum_texture_bytes(root))
                                    .unwrap_or(0)
                            });
                    if change >= 0 {
                        *tile_bytes = tile_bytes.saturating_add(change as u64);
                    } else {
                        *tile_bytes = tile_bytes.saturating_sub(change.unsigned_abs());
                    }
                }
                _ => {}
            }
        }
        crate::lease::BudgetDelta {
            delta_tiles,
            max_nodes_in_batch,
            delta_texture_bytes,
        }
    }

    /// Check if a mutation batch would exceed the lease's resource budget.
    ///
    /// Returns Ok(()) if within budget, or Err with the specific violation.
    pub fn check_budget(
        &self,
        lease_id: &SceneId,
        batch: &crate::mutation::MutationBatch,
    ) -> Result<(), BudgetError> {
        let lease = match self.leases.get(lease_id) {
            Some(l) => l,
            None => return Ok(()), // No lease = no budget to check
        };
        let budget = &lease.resource_budget;
        let usage = self.lease_resource_usage(lease_id);

        let budget_delta = self.mutation_budget_delta(lease_id, batch);
        let projected_tiles = if budget_delta.delta_tiles >= 0 {
            usage.tiles.saturating_add(budget_delta.delta_tiles as u32)
        } else {
            usage
                .tiles
                .saturating_sub(budget_delta.delta_tiles.unsigned_abs())
        };
        if projected_tiles > budget.max_tiles {
            return Err(BudgetError {
                resource: "tiles".to_string(),
                current: usage.tiles as u64,
                limit: budget.max_tiles as u64,
                requested: budget_delta.delta_tiles.max(0) as u64,
            });
        }

        let projected_texture_bytes = if budget_delta.delta_texture_bytes >= 0 {
            usage
                .texture_bytes
                .saturating_add(budget_delta.delta_texture_bytes as u64)
        } else {
            usage
                .texture_bytes
                .saturating_sub(budget_delta.delta_texture_bytes.unsigned_abs())
        };
        if projected_texture_bytes > budget.max_texture_bytes {
            return Err(BudgetError {
                resource: "texture_bytes".to_string(),
                current: usage.texture_bytes,
                limit: budget.max_texture_bytes,
                requested: budget_delta.delta_texture_bytes.max(0) as u64,
            });
        }

        // Count nodes against a virtual post-mutation view so multiple node
        // operations on one tile cannot bypass or overstate the ceiling.
        let mut projected_node_counts = usage.nodes_per_tile.clone();
        for mutation in &batch.mutations {
            match mutation {
                crate::mutation::SceneMutation::AddNode { tile_id, node, .. } => {
                    let added = Self::fresh_batch_node_count(node);
                    let projected = projected_node_counts
                        .entry(*tile_id)
                        .or_insert_with(|| usage.nodes_per_tile.get(tile_id).copied().unwrap_or(0));
                    *projected = projected.saturating_add(added);
                    if *projected > budget.max_nodes_per_tile {
                        return Err(BudgetError {
                            resource: "nodes_per_tile".to_string(),
                            current: projected.saturating_sub(added) as u64,
                            limit: budget.max_nodes_per_tile as u64,
                            requested: added as u64,
                        });
                    }
                }
                crate::mutation::SceneMutation::SetTileRoot {
                    tile_id,
                    node,
                    descendants,
                } => {
                    // SetTileRoot replaces the entire tree, so count new tree size.
                    // With an inline subtree (hud-ga4md) the root plus every fresh
                    // descendant counts against the per-tile node budget.
                    let new_count = Self::fresh_batch_node_count(node)
                        .saturating_add(u32::try_from(descendants.len()).unwrap_or(u32::MAX));
                    projected_node_counts.insert(*tile_id, new_count);
                    if new_count > budget.max_nodes_per_tile {
                        return Err(BudgetError {
                            resource: "nodes_per_tile".to_string(),
                            current: 0,
                            limit: budget.max_nodes_per_tile as u64,
                            requested: new_count as u64,
                        });
                    }
                }
                crate::mutation::SceneMutation::DeleteTile { tile_id } => {
                    projected_node_counts.remove(tile_id);
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Check if a lease is at the soft budget warning threshold (80%).
    pub fn is_lease_budget_warning(&self, lease_id: &SceneId) -> bool {
        let lease = match self.leases.get(lease_id) {
            Some(l) => l,
            None => return false,
        };
        let usage = self.lease_resource_usage(lease_id);
        let budget = &lease.resource_budget;

        let tile_pct = usage.tiles as f64 / budget.max_tiles.max(1) as f64;
        let tex_pct = usage.texture_bytes as f64 / budget.max_texture_bytes.max(1) as f64;

        tile_pct >= Self::BUDGET_SOFT_LIMIT_PCT || tex_pct >= Self::BUDGET_SOFT_LIMIT_PCT
    }

    /// Count nodes in a tile by walking the root node tree.
    pub(super) fn count_nodes_in_tile(&self, tile: &Tile) -> u32 {
        match tile.root_node {
            Some(root_id) => self.count_node_subtree(root_id),
            None => 0,
        }
    }

    pub(crate) fn count_node_subtree(&self, node_id: SceneId) -> u32 {
        let mut visited = HashSet::new();
        self.count_node_subtree_inner(node_id, &mut visited)
    }

    fn count_node_subtree_inner(&self, node_id: SceneId, visited: &mut HashSet<SceneId>) -> u32 {
        if !visited.insert(node_id) {
            // Cycle detected — skip this node to avoid infinite recursion.
            #[cfg(debug_assertions)]
            eprintln!(
                "[tze_hud_scene] cycle detected in node graph at {node_id:?} during count_node_subtree"
            );
            return 0;
        }
        match self.nodes.get(&node_id) {
            Some(node) => {
                1 + node
                    .children
                    .iter()
                    .map(|c| self.count_node_subtree_inner(*c, visited))
                    .sum::<u32>()
            }
            None => 0,
        }
    }

    pub(super) fn sum_texture_bytes(&self, node_id: SceneId) -> u64 {
        let mut visited = HashSet::new();
        self.sum_texture_bytes_inner(node_id, &mut visited)
    }

    fn sum_texture_bytes_inner(&self, node_id: SceneId, visited: &mut HashSet<SceneId>) -> u64 {
        if !visited.insert(node_id) {
            // Cycle detected — skip this node to avoid infinite recursion.
            #[cfg(debug_assertions)]
            eprintln!(
                "[tze_hud_scene] cycle detected in node graph at {node_id:?} during sum_texture_bytes"
            );
            return 0;
        }
        match self.nodes.get(&node_id) {
            Some(node) => {
                let self_bytes = match &node.data {
                    NodeData::StaticImage(img) => img.decoded_bytes,
                    _ => 0,
                };
                self_bytes
                    + node
                        .children
                        .iter()
                        .map(|c| self.sum_texture_bytes_inner(*c, visited))
                        .sum::<u64>()
            }
            None => 0,
        }
    }

    /// Number of nodes a not-yet-inserted node contributes to its tile when
    /// submitted as part of a fresh mutation batch.
    ///
    /// This is **always 1** by construction, not a tree walk: in the current
    /// node model a `Node`'s children are `SceneId` references, and in a fresh
    /// batch each child arrives as its own separate `AddNode`/`SetTileRoot`
    /// mutation. The incoming node therefore adds exactly itself; its children
    /// are budgeted when their own mutations are processed.
    ///
    /// Contrast with [`SceneGraph::count_node_tree_deep`], which *does* walk
    /// children that already exist in the graph (used by `set_tile_root_impl`
    /// to catch re-attachment of a persisted subtree).
    fn fresh_batch_node_count(_node: &Node) -> u32 {
        1
    }

    /// Count the incoming node plus any of its children that are already in the graph.
    ///
    /// Used by `set_tile_root_impl` to validate the post-insert node count before
    /// replacing the tile root. The incoming `node` is counted as 1, and any of its
    /// `children` SceneIds that already exist in `self.nodes` are recursively counted.
    ///
    /// For a brand-new node with no pre-existing children, this returns 1 (correct).
    /// For a node whose `children` already reference persisted nodes (e.g., re-attaching
    /// an existing subtree), this returns the full subtree size, preventing the node
    /// count limit from being bypassed.
    pub(super) fn count_node_tree_deep(&self, node: &Node) -> usize {
        1 + node
            .children
            .iter()
            .map(|child_id| self.count_node_subtree(*child_id) as usize)
            .sum::<usize>()
    }

    /// Count texture bytes in a node (not yet inserted into the graph).
    fn count_texture_bytes_in_node(node: &Node) -> u64 {
        match &node.data {
            NodeData::StaticImage(img) => img.decoded_bytes,
            _ => 0,
        }
    }

    /// Budget-driven revocation: transitions all non-terminal session leases to
    /// REVOKED, clears tiles, clears zone publications.
    ///
    /// Spec §Post-Revocation Resource Cleanup (lines 253–260):
    /// - Bypasses the grace period entirely.
    /// - Caller is responsible for sending `LeaseResponse{revoke_reason=BUDGET_POLICY}`
    ///   and then waiting `POST_REVOCATION_FREE_DELAY_MS` before calling
    ///   `finalize_budget_revocation`.
    ///
    /// Returns the cleanup spec (containing the free delay) for each revoked lease.
    pub fn initiate_budget_revocation(
        &mut self,
        session_namespace: &str,
    ) -> Vec<crate::lease::cleanup::PostRevocationCleanupSpec> {
        use crate::lease::cleanup::{PostRevocationCleanupSpec, RevocationKind};
        let now_ms = self.clock.now_millis();

        // Collect all non-terminal leases for this namespace.
        let to_revoke: Vec<SceneId> = self
            .leases
            .values()
            .filter(|l| l.namespace == session_namespace && !l.state.is_terminal())
            .map(|l| l.id)
            .collect();

        let mut specs = Vec::new();
        for lease_id in to_revoke {
            // Transition to REVOKED (bypasses grace — no orphan path).
            if let Some(lease) = self.leases.get_mut(&lease_id) {
                lease.state = LeaseState::Revoked;
            }
            // Tiles will be freed after the 100ms delay by finalize_budget_revocation.
            // Budget revocation bypasses the orphan/disconnection path, so tiles
            // do not receive a DisconnectionBadge — they are simply marked for
            // pending removal (visual_hint remains None; compositor will not render
            // them once removed by finalize_budget_revocation).

            // Clear zone and widget publications immediately on REVOKED transition.
            // Spec §Requirement: Lease Revocation Clears Zone Publications
            // (lines 235–242): zone pubs must be cleared when lease is REVOKED/EXPIRED.
            // Widget publications are similarly cleared immediately.
            // Tile/node resources are deferred by the 100ms delay; zone/widget pubs are not.
            if let Some(lease) = self.leases.get(&lease_id) {
                let ns = lease.namespace.clone();
                self.clear_zone_publications_for_namespace(&ns);
                self.clear_widget_publications_for_namespace(&ns);
            }
            specs.push(PostRevocationCleanupSpec::new(
                lease_id,
                session_namespace,
                RevocationKind::BudgetPolicy,
                now_ms,
            ));
        }

        if !specs.is_empty() {
            self.version += 1;
        }
        specs
    }

    /// Finalize budget revocation: remove tiles and zone publications for
    /// all leases in the cleanup specs that are ready to free.
    ///
    /// Must be called after `POST_REVOCATION_FREE_DELAY_MS` has elapsed
    /// (spec line 254: "free all resources after a 100ms delay").
    ///
    /// Returns the number of specs that were finalized.
    pub fn finalize_budget_revocation(
        &mut self,
        specs: &[crate::lease::cleanup::PostRevocationCleanupSpec],
        now_ms: u64,
    ) -> usize {
        let mut finalized = 0;
        for spec in specs {
            if spec.is_ready_to_free(now_ms) {
                // Remove tiles
                let tile_ids: Vec<SceneId> = self
                    .tiles
                    .values()
                    .filter(|t| t.lease_id == spec.lease_id)
                    .map(|t| t.id)
                    .collect();
                for tid in tile_ids {
                    self.remove_tile_and_nodes(tid);
                }
                // Clear zone and widget publications
                self.clear_zone_publications_for_namespace(&spec.session_namespace);
                self.clear_widget_publications_for_namespace(&spec.session_namespace);
                finalized += 1;
            }
        }
        if finalized > 0 {
            self.version += 1;
        }
        finalized
    }
}
