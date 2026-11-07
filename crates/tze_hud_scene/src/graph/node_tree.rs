use super::SceneGraph;
use crate::types::*;
use std::collections::HashMap;

impl SceneGraph {
    // ─── Node tree helpers ───────────────────────────────────────────────

    /// Register the per-node side effects an inserted node owns: image-resource
    /// ref counting and hit-region local input state. Centralized here (rather
    /// than duplicated at each `insert_node_tree` call site) so every node of a
    /// materialized subtree — root AND inline descendants (hud-ga4md) — is
    /// treated identically. `remove_node_tree` unwinds both (dec ref + drop
    /// hit-region state) symmetrically.
    fn register_inserted_node(&mut self, node: &Node) {
        match node.data {
            NodeData::StaticImage(ref si) => self.inc_resource_ref(si.resource_id),
            NodeData::HitRegion(_) => {
                self.hit_region_states
                    .insert(node.id, HitRegionLocalState::new(node.id));
            }
            _ => {}
        }
    }

    /// Materialize a node subtree into the flat node map (hud-ga4md).
    ///
    /// `node` is inserted, its side effects registered, and each SceneId in
    /// `node.children` is looked up in `descendants` and materialized
    /// recursively (depth-first). `descendants` is the flat id→node map of every
    /// node BELOW some root (produced from the wire's inline `NodeProto.children`
    /// by `convert::proto_node_tree_to_scene`).
    ///
    /// A child id absent from `descendants` is left as a dangling ref rather than
    /// aborting — this preserves the legacy re-attach case where `node.children`
    /// reference nodes ALREADY present in `self.nodes` (they are not re-inserted,
    /// exactly as before this became recursive). For the common single-node
    /// insert (`AddNode`, or a flat tile root), pass an empty map: no child is
    /// materialized and behavior is byte-identical to the pre-subtree stub.
    pub(super) fn insert_node_tree(&mut self, node: &Node, descendants: &HashMap<SceneId, Node>) {
        self.register_inserted_node(node);
        self.nodes.insert(node.id, node.clone());
        for child_id in &node.children {
            if let Some(child) = descendants.get(child_id) {
                self.insert_node_tree(child, descendants);
            }
        }
    }

    pub(crate) fn remove_node_tree(&mut self, node_id: SceneId) {
        if let Some(node) = self.nodes.remove(&node_id) {
            // Decrement the resource ref count if this node referenced an image resource.
            if let NodeData::StaticImage(ref si) = node.data {
                self.dec_resource_ref(&si.resource_id);
            }
            for child_id in &node.children {
                self.remove_node_tree(*child_id);
            }
        }
        self.hit_region_states.remove(&node_id);
    }

    /// Scale a viewer-locked tile's freshly-published subtree (rooted at
    /// `subtree_root`) so its tile-local extent tracks the tile's viewer-defined
    /// content bounds (hud-rpmwt).
    ///
    /// Once the viewer takes geometry authority over a portal member via a
    /// whole-portal resize (`is_viewer_geometry_locked`), the tile-bounds lock
    /// (hud-lyqun) already ignores adapter-originated `UpdateTileBounds`. But a
    /// content republish rebuilds the node tree — `set_tile_root` installs the
    /// transcript, then a separate `AddNode` attaches the composer hit region —
    /// and the compositor wraps `TextMarkdownNode` text to `node.bounds.width`
    /// and clips to `tile.bounds`. An adapter that republishes its stale
    /// attach-time (config) node geometry would therefore re-home the content
    /// back to the old width — overflowing the resized pane *unclipped* and
    /// wrapping at the stale width (the exact hud-rpmwt live repro). Reconciling
    /// each published subtree to the tile bounds — the same scaling
    /// `scale_tile_node_tree` applies at resize time — makes the tile the single
    /// wrap-width / hit-geometry authority after a viewer resize.
    ///
    /// Called per republished subtree (the [`set_tile_root`] root AND each later
    /// `add_node_to_tile` child), because both may arrive in one batch and the
    /// `AddNode` child is not present when the root is installed. Every portal
    /// node is published tile-filling in the adapter's (possibly stale)
    /// coordinate space, so each subtree is reconciled independently by its own
    /// `tile / published-extent` ratio: order-independent and free of
    /// double-scaling — a subtree already tracking the tile scales by ~1.0 and is
    /// skipped. Scoped to viewer-locked tiles, so ordinary agent tiles are
    /// untouched.
    ///
    /// Node bounds are tile-local; scaling (not clamping) preserves relative
    /// child layout, matching the resize path. A no-op when the tile is not
    /// viewer-locked, when the subtree already tracks the tile, or when either
    /// the subtree root or the tile has a non-positive extent (NaN-safe: the
    /// positive-form guards reject non-finite values, so no infinity/NaN reaches
    /// the scaling walk).
    ///
    /// [`set_tile_root`]: SceneGraph::set_tile_root
    pub(super) fn reconcile_locked_subtree_to_tile_bounds(
        &mut self,
        tile_id: SceneId,
        subtree_root: SceneId,
    ) {
        if !self.is_viewer_geometry_locked(tile_id) {
            return;
        }
        let Some(tile_bounds) = self.tiles.get(&tile_id).map(|t| t.bounds) else {
            return;
        };
        let Some(root_bounds) = self.nodes.get(&subtree_root).map(|n| n.data.bounds()) else {
            return;
        };
        // Accept only finite, strictly-positive extents. Rejecting NaN here means
        // the division below never yields an infinity, and the ratio guard below
        // stays a plain method-call negation (no partial-ord comparison).
        let finite_positive = |v: f32| v.is_finite() && v > 0.0;
        if !finite_positive(root_bounds.width) || !finite_positive(root_bounds.height) {
            return;
        }
        let r_w = tile_bounds.width / root_bounds.width;
        let r_h = tile_bounds.height / root_bounds.height;
        // Reject a degenerate tile extent (non-positive / non-finite ratio) —
        // leave the subtree untouched rather than collapse or invert it.
        if !finite_positive(r_w) || !finite_positive(r_h) {
            return;
        }
        // Adapter already published resize-aware bounds → nothing to reconcile.
        // Guards against needless float churn and a tree walk on the common path.
        if (r_w - 1.0).abs() < f32::EPSILON && (r_h - 1.0).abs() < f32::EPSILON {
            return;
        }
        // Collect the subtree ids with an immutable walk first, then mutate —
        // mirrors `scale_tile_node_tree` in the runtime resize path so the two
        // node-scaling sites stay behaviourally identical. The tree is small
        // (≤ MAX_NODES_PER_TILE) and republish is a coalesced state-stream event,
        // not a hot loop.
        let mut stack = vec![subtree_root];
        let mut ids = Vec::new();
        while let Some(id) = stack.pop() {
            let Some(node) = self.nodes.get(&id) else {
                continue;
            };
            ids.push(id);
            stack.extend(node.children.iter().copied());
        }
        for id in ids {
            if let Some(node) = self.nodes.get_mut(&id) {
                let b = node.data.bounds_mut();
                b.x *= r_w;
                b.y *= r_h;
                b.width *= r_w;
                b.height *= r_h;
            }
        }
    }

    pub(crate) fn remove_tile_and_nodes(&mut self, tile_id: SceneId) {
        if let Some(tile) = self.tiles.remove(&tile_id)
            && let Some(root_id) = tile.root_node
        {
            self.remove_node_tree(root_id);
        }
        self.overlay.tile_scroll_configs.remove(&tile_id);
        self.overlay.tile_scroll_offsets.remove(&tile_id);
        self.overlay.portal_input_scroll_offsets.remove(&tile_id);
        self.overlay.portal_scroll_origin_epochs.remove(&tile_id);
        self.overlay
            .portal_document_layouts
            .retain(|(tile, _), _| *tile != tile_id);
        self.overlay.tile_follow_tail_at_tail.remove(&tile_id);
        self.overlay.tile_unread_counts.remove(&tile_id);
        self.overlay.tile_lifecycle_accents.remove(&tile_id);
        // hud-iofav: the derived composer hit-region node is dropped with the root
        // subtree above; prune the tile's composer-interaction overlay + node map.
        self.overlay.tile_composer_interactions.remove(&tile_id);
        self.overlay.tile_composer_nodes.remove(&tile_id);
        self.overlay.portal_surfaces.remove(&tile_id);
        self.overlay.drag_active_elements.remove(&tile_id);
        self.overlay.viewer_geometry_locked.remove(&tile_id);
        self.overlay.tile_font_scale.remove(&tile_id);
        // Notify the windowed runtime so it can eagerly prune per-tile state
        // that cannot live in the scene graph (e.g. `portal_resize_states`).
        // Drained by `SceneGraph::drain_removed_tile_ids` in `about_to_wait`.
        self.overlay.recently_removed_tile_ids.push(tile_id);
    }
}
