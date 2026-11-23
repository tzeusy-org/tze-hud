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
}
