use super::*;

impl SceneGraph {
    // ─── Tile operations ─────────────────────────────────────────────────

    /// Create a tile. This is the unchecked form used internally for scene construction.
    ///
    /// For agent-facing operations use [`create_tile_checked`] which enforces:
    /// - Lease active + `CreateTiles` + `ModifyOwnTiles` capabilities
    /// - Per-tab tile count limit (1024)
    /// - Bounds positive-size and within-display-area
    /// - z_order < ZONE_TILE_Z_MIN
    pub fn create_tile(
        &mut self,
        tab_id: SceneId,
        namespace: &str,
        lease_id: SceneId,
        bounds: Rect,
        z_order: u32,
    ) -> Result<SceneId, ValidationError> {
        self.create_tile_impl(tab_id, namespace, lease_id, bounds, z_order, false)
    }

    /// Create a tile with full spec-compliant validation including capability checks.
    ///
    /// RFC 0001 §2.3, §3.1, §3.3: requires active lease, `create_tiles`, and
    /// `modify_own_tiles` capabilities. Enforces per-tab tile limit, bounds invariants,
    /// and z_order zone-band reservation.
    pub fn create_tile_checked(
        &mut self,
        tab_id: SceneId,
        namespace: &str,
        lease_id: SceneId,
        bounds: Rect,
        z_order: u32,
    ) -> Result<SceneId, ValidationError> {
        self.create_tile_impl(tab_id, namespace, lease_id, bounds, z_order, true)
    }

    fn create_tile_impl(
        &mut self,
        tab_id: SceneId,
        namespace: &str,
        lease_id: SceneId,
        bounds: Rect,
        z_order: u32,
        enforce_capabilities: bool,
    ) -> Result<SceneId, ValidationError> {
        // Validate tab exists
        if !self.tabs.contains_key(&tab_id) {
            return Err(ValidationError::TabNotFound { id: tab_id });
        }

        if enforce_capabilities {
            // Lease must be active and have create_tiles + modify_own_tiles
            self.require_active_lease(lease_id)?;
            self.require_capability(lease_id, Capability::CreateTiles)?;
            self.require_capability(lease_id, Capability::ModifyOwnTiles)?;

            // Namespace isolation: the caller's namespace must match the lease's namespace.
            // This prevents an agent from creating tiles in another agent's namespace
            // using their own (valid) lease. RFC 0001 §1.2.
            let lease_namespace = self
                .leases
                .get(&lease_id)
                .map(|l| l.namespace.as_str())
                .unwrap_or("");
            if namespace != lease_namespace {
                return Err(ValidationError::NamespaceMismatch {
                    tile_id: lease_id, // use lease_id as context; tile not created yet
                    tile_namespace: lease_namespace.to_string(),
                    agent_namespace: namespace.to_string(),
                });
            }
        } else {
            // Validate lease exists at minimum
            if !self.leases.contains_key(&lease_id) {
                return Err(ValidationError::LeaseNotFound { id: lease_id });
            }
        }

        // Per-tab tile count limit (RFC 0001 §2.1: max 1024 tiles per tab)
        let tiles_in_tab = self.tiles.values().filter(|t| t.tab_id == tab_id).count();
        if tiles_in_tab >= MAX_TILES_PER_TAB {
            return Err(ValidationError::BudgetExceeded {
                resource: format!("tiles_per_tab (limit {MAX_TILES_PER_TAB})"),
            });
        }

        // Bounds: width and height must be > 0 (RFC 0001 §2.3)
        if bounds.width <= 0.0 || bounds.height <= 0.0 {
            return Err(ValidationError::BoundsOutOfRange {
                reason: format!(
                    "tile bounds width ({}) and height ({}) must be > 0.0",
                    bounds.width, bounds.height
                ),
            });
        }

        // Bounds must be fully within the tab display area (RFC 0001 §2.3)
        if !bounds.is_within(&self.display_area) {
            return Err(ValidationError::BoundsOutOfRange {
                reason: format!(
                    "tile bounds ({},{} {}×{}) are not fully within display area ({},{} {}×{})",
                    bounds.x,
                    bounds.y,
                    bounds.width,
                    bounds.height,
                    self.display_area.x,
                    self.display_area.y,
                    self.display_area.width,
                    self.display_area.height,
                ),
            });
        }

        // z_order must be < ZONE_TILE_Z_MIN for agent-owned tiles (RFC 0001 §2.3)
        if z_order >= ZONE_TILE_Z_MIN {
            return Err(ValidationError::InvalidField {
                field: "z_order".into(),
                reason: format!(
                    "z_order 0x{z_order:08X} is >= ZONE_TILE_Z_MIN (0x{ZONE_TILE_Z_MIN:08X}); reserved for runtime zone tiles"
                ),
            });
        }

        let id = SceneId::new();
        self.tiles.insert(
            id,
            Tile {
                id,
                tab_id,
                namespace: namespace.to_string(),
                lease_id,
                bounds,
                z_order,
                opacity: 1.0,
                input_mode: InputMode::Capture,
                present_at: None,
                expires_at: None,
                resource_budget: ResourceBudget::default(),
                root_node: None,
                visual_hint: crate::lease::TileVisualHint::None,
            },
        );
        self.version += 1;
        Ok(id)
    }

    /// Update the bounds of a tile.
    ///
    /// RFC 0001 §2.3: requires active lease + `ModifyOwnTiles` capability.
    /// Bounds must be positive and within the display area.
    pub fn update_tile_bounds(
        &mut self,
        tile_id: SceneId,
        bounds: Rect,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.get_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_capability(lease_id, Capability::ModifyOwnTiles)?;

        // Viewer geometry authority (hud-lyqun): once the viewer has moved or
        // resized this tile as part of a whole-portal gesture, the adapter no
        // longer controls its bounds. Silently accept the mutation but leave the
        // bounds untouched — the adapter's content updates still apply within the
        // viewer-defined geometry, but its stale client-side layout can never
        // reposition the member and fracture the portal group. Viewer-driven
        // resize/drag write `tile.bounds` directly and so are not affected by
        // this gate; only adapter-originated `UpdateTileBounds` reaches here.
        if self.is_viewer_geometry_locked(tile_id) {
            return Ok(());
        }

        if bounds.width <= 0.0 || bounds.height <= 0.0 {
            return Err(ValidationError::BoundsOutOfRange {
                reason: format!(
                    "tile bounds width ({}) and height ({}) must be > 0.0",
                    bounds.width, bounds.height
                ),
            });
        }
        if !bounds.is_within(&self.display_area) {
            return Err(ValidationError::BoundsOutOfRange {
                reason: format!(
                    "tile bounds ({},{} {}×{}) are not fully within display area",
                    bounds.x, bounds.y, bounds.width, bounds.height
                ),
            });
        }

        let tile = self
            .tiles
            .get_mut(&tile_id)
            .expect("tile_id existence verified by get_tile_lease_checked");
        tile.bounds = bounds;
        self.version += 1;
        Ok(())
    }

    /// Update the z-order of a tile.
    ///
    /// RFC 0001 §2.3: requires active lease + `ModifyOwnTiles`.
    /// z_order must be < ZONE_TILE_Z_MIN.
    pub fn update_tile_z_order(
        &mut self,
        tile_id: SceneId,
        z_order: u32,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.get_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_capability(lease_id, Capability::ModifyOwnTiles)?;

        if z_order >= ZONE_TILE_Z_MIN {
            return Err(ValidationError::InvalidField {
                field: "z_order".into(),
                reason: format!(
                    "z_order 0x{z_order:08X} is >= ZONE_TILE_Z_MIN (0x{ZONE_TILE_Z_MIN:08X}); reserved for runtime zone tiles"
                ),
            });
        }

        let tile = self
            .tiles
            .get_mut(&tile_id)
            .expect("tile_id existence verified by get_tile_lease_checked");
        tile.z_order = z_order;
        self.version += 1;
        Ok(())
    }

    /// Update the opacity of a tile.
    ///
    /// RFC 0001 §2.3: opacity must be in [0.0, 1.0]. Requires active lease + `ModifyOwnTiles`.
    pub fn update_tile_opacity(
        &mut self,
        tile_id: SceneId,
        opacity: f32,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.get_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_capability(lease_id, Capability::ModifyOwnTiles)?;

        if !(0.0..=1.0).contains(&opacity) {
            return Err(ValidationError::InvalidField {
                field: "opacity".into(),
                reason: format!("opacity {opacity} is not in [0.0, 1.0]"),
            });
        }

        let tile = self
            .tiles
            .get_mut(&tile_id)
            .expect("tile_id existence verified by get_tile_lease_checked");
        tile.opacity = opacity;
        self.version += 1;
        Ok(())
    }

    /// Update the input mode of a tile.
    ///
    /// RFC 0001 §2.3: requires active lease + `ModifyOwnTiles`.
    pub fn update_tile_input_mode(
        &mut self,
        tile_id: SceneId,
        input_mode: InputMode,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.get_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_capability(lease_id, Capability::ModifyOwnTiles)?;

        let tile = self
            .tiles
            .get_mut(&tile_id)
            .expect("tile_id existence verified by get_tile_lease_checked");
        tile.input_mode = input_mode;
        self.version += 1;
        Ok(())
    }

    /// Update the expiry timestamp of a tile.
    ///
    /// RFC 0001 §2.3: requires active lease + `ModifyOwnTiles`.
    pub fn update_tile_expiry(
        &mut self,
        tile_id: SceneId,
        expires_at: Option<u64>,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.get_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_capability(lease_id, Capability::ModifyOwnTiles)?;

        let tile = self
            .tiles
            .get_mut(&tile_id)
            .expect("tile_id existence verified by get_tile_lease_checked");
        tile.expires_at = expires_at;
        self.version += 1;
        Ok(())
    }

    /// Delete a tile and all its nodes.
    ///
    /// RFC 0001 §2.3: requires active lease + `ModifyOwnTiles`. Namespace isolation enforced.
    pub fn delete_tile(
        &mut self,
        tile_id: SceneId,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.get_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_capability(lease_id, Capability::ModifyOwnTiles)?;

        self.remove_tile_and_nodes(tile_id);
        self.version += 1;
        Ok(())
    }

    /// Get the lease ID for a tile, enforcing namespace isolation.
    ///
    /// Returns `NamespaceMismatch` if the tile belongs to a different namespace.
    /// Returns `TileNotFound` if the tile does not exist.
    fn get_tile_lease_checked(
        &self,
        tile_id: SceneId,
        agent_namespace: &str,
    ) -> Result<SceneId, ValidationError> {
        let tile = self
            .tiles
            .get(&tile_id)
            .ok_or(ValidationError::TileNotFound { id: tile_id })?;
        if tile.namespace != agent_namespace {
            return Err(ValidationError::NamespaceMismatch {
                tile_id,
                tile_namespace: tile.namespace.clone(),
                agent_namespace: agent_namespace.to_string(),
            });
        }
        Ok(tile.lease_id)
    }

    pub fn set_tile_root(&mut self, tile_id: SceneId, node: Node) -> Result<(), ValidationError> {
        self.set_tile_root_impl(tile_id, node, Vec::new(), None)
    }

    /// Set tile root with full capability and node-count enforcement.
    pub fn set_tile_root_checked(
        &mut self,
        tile_id: SceneId,
        node: Node,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        self.set_tile_root_impl(tile_id, node, Vec::new(), Some(agent_namespace))
    }

    /// Set a tile root together with an inline descendant subtree, materialized
    /// atomically (hud-ga4md).
    ///
    /// `node` is the root; `descendants` is the flat list of every node below it
    /// (any depth, root excluded), as produced by
    /// `convert::proto_node_tree_to_scene`. The root's `children` (and each
    /// descendant's `children`) reference the descendants by SceneId. The whole
    /// subtree is validated and inserted in a single call — the point being that
    /// a multi-node portal body arrives as ONE `SetTileRoot`/`PublishToTile`
    /// mutation and stays coalescible StateStream, never a per-node `AddNode`
    /// fan-out that would break republish latest-wins coalescing (hud-mzk74).
    ///
    /// Passing an empty `descendants` is exactly [`set_tile_root`](Self::set_tile_root).
    pub fn set_tile_root_tree(
        &mut self,
        tile_id: SceneId,
        node: Node,
        descendants: Vec<Node>,
    ) -> Result<(), ValidationError> {
        self.set_tile_root_impl(tile_id, node, descendants, None)
    }

    /// Subtree-aware [`set_tile_root_checked`](Self::set_tile_root_checked):
    /// enforces the lease + `ModifyOwnTiles` capability, then materializes the
    /// root and its inline `descendants` atomically (hud-ga4md).
    pub fn set_tile_root_tree_checked(
        &mut self,
        tile_id: SceneId,
        node: Node,
        descendants: Vec<Node>,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        self.set_tile_root_impl(tile_id, node, descendants, Some(agent_namespace))
    }

    fn set_tile_root_impl(
        &mut self,
        tile_id: SceneId,
        node: Node,
        descendants: Vec<Node>,
        agent_namespace: Option<&str>,
    ) -> Result<(), ValidationError> {
        if let Some(ns) = agent_namespace {
            let lease_id = self.get_tile_lease_checked(tile_id, ns)?;
            self.require_active_lease(lease_id)?;
            self.require_capability(lease_id, Capability::ModifyOwnTiles)?;
        }

        // Validate the ENTIRE incoming subtree (root + inline descendants,
        // hud-ga4md) BEFORE mutating anything, so an invalid subtree leaves the
        // graph untouched (atomic materialization). `all_incoming` walks the root
        // first, then every descendant.
        let all_incoming = || std::iter::once(&node).chain(descendants.iter());

        // Check for duplicate node IDs (scene-globally unique per RFC 0001 §2.1):
        // no incoming id may already exist in the graph, and no two incoming
        // nodes may share an id.
        let mut seen_ids: std::collections::HashSet<SceneId> =
            std::collections::HashSet::with_capacity(1 + descendants.len());
        for n in all_incoming() {
            if self.nodes.contains_key(&n.id) || !seen_ids.insert(n.id) {
                return Err(ValidationError::DuplicateId { id: n.id });
            }
        }

        // Validate node data constraints (e.g. TextMarkdownNode content size limit)
        // for every node in the subtree.
        for n in all_incoming() {
            if let Some(err) = validate_text_markdown_node_data(&n.data) {
                return Err(err);
            }
        }

        // Enforce resource registration for agent-submitted StaticImageNode
        // mutations across the whole subtree. Same gate as add_node_to_tile_impl;
        // see that function's comment for spec refs.
        if agent_namespace.is_some() {
            for n in all_incoming() {
                if let NodeData::StaticImage(ref si) = n.data {
                    if !self.registered_resources.contains_key(&si.resource_id) {
                        return Err(ValidationError::ResourceNotFound { id: si.resource_id });
                    }
                }
            }
        }

        // Node count limit: SetTileRoot replaces the whole tree. `count_node_tree_deep`
        // counts the root plus any children ALREADY in the graph (legacy re-attach);
        // every fresh inline descendant adds one more (they are not yet in the graph).
        let incoming_count = self.count_node_tree_deep(&node) + descendants.len();
        if incoming_count > MAX_NODES_PER_TILE {
            return Err(ValidationError::NodeCountExceeded {
                tile_id,
                current: incoming_count,
                limit: MAX_NODES_PER_TILE,
            });
        }

        // Get old root first, then release the borrow
        let old_root = {
            let tile = self
                .tiles
                .get(&tile_id)
                .ok_or(ValidationError::TileNotFound { id: tile_id })?;
            tile.root_node
        };

        // Remove old root and its subtree if present
        if let Some(old_root_id) = old_root {
            self.remove_node_tree(old_root_id);
        }

        let node_id = node.id;

        // Materialize the root and its inline descendants recursively. Per-node
        // side effects (resource ref counts, hit-region local state) are
        // registered inside insert_node_tree for every node in the subtree.
        let descendant_map: std::collections::HashMap<SceneId, Node> =
            descendants.into_iter().map(|n| (n.id, n)).collect();
        self.insert_node_tree(&node, &descendant_map);

        // Set the new root on the tile
        let tile = self
            .tiles
            .get_mut(&tile_id)
            .expect("tile_id existence verified earlier in set_tile_root_impl");
        tile.root_node = Some(node_id);

        // Replacing the root subtree removes the previous node tree, so any portal
        // surface part still pointing at a removed node would dangle. Prune those
        // stale refs back to `None` (the adapter re-binds on its next
        // SetPortalSurface) so consumers never resolve a stale SceneId
        // (hud-tc153 review P2).
        self.revalidate_portal_surface_part_nodes(tile_id);

        // Node-bounds authority after a viewer whole-portal resize (hud-rpmwt):
        // if the viewer has taken geometry authority over this tile, reconcile the
        // just-published root subtree to the tile's resized bounds so an adapter's
        // stale-wide content cannot re-home the transcript back to its attach-time
        // wrap width. Scoped to viewer-locked tiles, so ordinary agent tiles are
        // untouched. See `reconcile_locked_subtree_to_tile_bounds`.
        self.reconcile_locked_subtree_to_tile_bounds(tile_id, node_id);

        // Re-attach the composer interaction hit region as a derived consequence of
        // the tile's stored composer-interaction overlay state (hud-iofav). The old
        // root subtree removal above wiped any prior derived node, so an
        // interaction-enabled portal's composer would vanish on this republish
        // without this reattach. Deriving it here — rather than requiring the
        // adapter to re-send a per-republish `AddNode` — is what keeps the batch
        // coalescible StateStream on the hottest path (streaming transcript with an
        // interactive composer, hud-mzk74 / hud-iofav).
        self.ensure_tile_composer_node(tile_id);

        self.version += 1;
        Ok(())
    }

    /// Reconcile the tile's derived composer hit-region node to its stored
    /// composer-interaction overlay spec (hud-iofav).
    ///
    /// Idempotent: if the tile has a composer spec and the current derived node is
    /// still valid (exists and is a child of the current root), this is a no-op.
    /// Otherwise a stale/missing derived node is detached and — when a spec and a
    /// root are both present — a fresh hit-region node is synthesized and attached
    /// under the root. Called after each root replacement
    /// ([`set_tile_root_impl`](Self::set_tile_root_impl)) and by the
    /// `SetTileComposerInteraction` apply path.
    ///
    /// The derived node is a real scene node so `hit_test`, focus acquisition, and
    /// the `ComposerDraftManager` all operate unchanged.
    pub(crate) fn ensure_tile_composer_node(&mut self, tile_id: SceneId) {
        let Some(region) = self
            .overlay
            .tile_composer_interactions
            .get(&tile_id)
            .cloned()
        else {
            // No spec: make sure no stale derived node lingers.
            self.detach_tile_composer_node(tile_id);
            return;
        };
        let Some(root_id) = self.tiles.get(&tile_id).and_then(|t| t.root_node) else {
            // No root yet: nothing to attach to. The node is (re)derived when the
            // next `SetTileRoot`/`PublishToTile` installs a root.
            self.detach_tile_composer_node(tile_id);
            return;
        };

        // Fast path: the current derived node is still valid (present and parented
        // by the current root) — leave it in place. This makes the second call in a
        // render batch (the `SetTileComposerInteraction` mutation, after the
        // `PublishToTile` reattach) a no-op when the spec did not change.
        if let Some(&current) = self.overlay.tile_composer_nodes.get(&tile_id) {
            let valid = self.nodes.contains_key(&current)
                && self
                    .nodes
                    .get(&root_id)
                    .is_some_and(|rn| rn.children.contains(&current));
            if valid {
                return;
            }
            self.detach_tile_composer_node(tile_id);
        }

        // Synthesize a fresh hit-region node from the spec and attach it under the
        // current root.
        let node_id = SceneId::new();
        self.nodes.insert(
            node_id,
            Node {
                layout: Default::default(),
                id: node_id,
                children: Vec::new(),
                data: NodeData::HitRegion(region),
            },
        );
        self.hit_region_states
            .insert(node_id, HitRegionLocalState::new(node_id));
        if let Some(root) = self.nodes.get_mut(&root_id) {
            root.children.push(node_id);
        }
        self.overlay.tile_composer_nodes.insert(tile_id, node_id);
    }

    /// Detach and drop the tile's derived composer hit-region node, if any
    /// (hud-iofav). Removes it from `nodes`, its parent's `children`, the
    /// hit-region local-state map, and the tile→node mapping. Safe to call when no
    /// derived node exists (or it was already removed with a prior root subtree).
    pub(crate) fn detach_tile_composer_node(&mut self, tile_id: SceneId) {
        let Some(node_id) = self.overlay.tile_composer_nodes.remove(&tile_id) else {
            return;
        };
        self.nodes.remove(&node_id);
        self.hit_region_states.remove(&node_id);
        if let Some(root_id) = self.tiles.get(&tile_id).and_then(|t| t.root_node) {
            if let Some(root) = self.nodes.get_mut(&root_id) {
                root.children.retain(|c| *c != node_id);
            }
        }
    }

    pub fn add_node_to_tile(
        &mut self,
        tile_id: SceneId,
        parent_id: Option<SceneId>,
        node: Node,
    ) -> Result<(), ValidationError> {
        self.add_node_to_tile_impl(tile_id, parent_id, node, None)
    }

    /// Add a node to a tile with full spec-compliant validation.
    pub fn add_node_to_tile_checked(
        &mut self,
        tile_id: SceneId,
        parent_id: Option<SceneId>,
        node: Node,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        self.add_node_to_tile_impl(tile_id, parent_id, node, Some(agent_namespace))
    }

    fn add_node_to_tile_impl(
        &mut self,
        tile_id: SceneId,
        parent_id: Option<SceneId>,
        node: Node,
        agent_namespace: Option<&str>,
    ) -> Result<(), ValidationError> {
        if let Some(ns) = agent_namespace {
            let lease_id = self.get_tile_lease_checked(tile_id, ns)?;
            self.require_active_lease(lease_id)?;
            self.require_capability(lease_id, Capability::ModifyOwnTiles)?;
        } else if !self.tiles.contains_key(&tile_id) {
            return Err(ValidationError::TileNotFound { id: tile_id });
        }

        // Check for duplicate node ID (RFC 0001 §2.1: NodeIds must be scene-globally unique)
        if self.nodes.contains_key(&node.id) {
            return Err(ValidationError::DuplicateId { id: node.id });
        }

        // Validate node data constraints (e.g. TextMarkdownNode content size limit)
        if let Some(err) = validate_text_markdown_node_data(&node.data) {
            return Err(err);
        }

        // Enforce resource registration for agent-submitted StaticImageNode mutations.
        //
        // Per spec resource-store/spec.md §Requirement: Resource Upload Before Tile
        // Creation: "Any agent-submitted tile mutation that references a ResourceId not
        // present in the resource store MUST be rejected."
        //
        // Only enforced for agent-submitted paths (agent_namespace.is_some()).
        // Internal/test paths (unchecked variants, snapshot restore) bypass this gate.
        if agent_namespace.is_some() {
            if let NodeData::StaticImage(ref si) = node.data {
                if !self.registered_resources.contains_key(&si.resource_id) {
                    return Err(ValidationError::ResourceNotFound { id: si.resource_id });
                }
            }
        }

        // Enforce per-tile node count limit (RFC 0001 §2.1: max 64 nodes)
        let current_count = self.count_nodes_in_tile(
            self.tiles
                .get(&tile_id)
                .expect("tile_id existence verified above in add_node_to_tile_impl"),
        ) as usize;
        if current_count >= MAX_NODES_PER_TILE {
            return Err(ValidationError::NodeCountExceeded {
                tile_id,
                current: current_count,
                limit: MAX_NODES_PER_TILE,
            });
        }

        let node_id = node.id;

        // If parent specified, add as child
        if let Some(pid) = parent_id {
            let parent = self
                .nodes
                .get_mut(&pid)
                .ok_or(ValidationError::NodeNotFound { id: pid })?;
            parent.children.push(node_id);
        } else {
            // Set as root if no root exists
            let tile = self
                .tiles
                .get_mut(&tile_id)
                .expect("tile_id existence verified above in add_node_to_tile_impl");
            if tile.root_node.is_none() {
                tile.root_node = Some(node_id);
            }
        }

        // Insert the single node; per-node side effects (hit-region local state,
        // image-resource ref count) are registered inside insert_node_tree. No
        // inline descendants on this path — AddNode adds one node at a time.
        self.insert_node_tree(&node, &std::collections::HashMap::new());

        // Node-bounds authority (hud-rpmwt): a portal render batch adds the
        // composer hit region as a separate `AddNode` after the transcript root,
        // so reconcile this newly-added subtree to the tile too — otherwise, on a
        // viewer-locked (resized) tile, a child published at stale attach-time
        // bounds keeps stale wrap / hit geometry while only the root was scaled.
        // Order-independent and no-double-scale (a subtree already tracking the
        // tile is skipped). Scoped to viewer-locked tiles.
        self.reconcile_locked_subtree_to_tile_bounds(tile_id, node_id);

        self.version += 1;
        Ok(())
    }

    /// Atomically replace the `data` of an existing node (unchecked form).
    ///
    /// The node must already exist in the scene graph and belong to `tile_id`.
    /// The replacement `data` discriminant must match the existing node's discriminant.
    pub fn update_node_content(
        &mut self,
        tile_id: SceneId,
        node_id: SceneId,
        data: NodeData,
    ) -> Result<(), ValidationError> {
        self.update_node_content_impl(tile_id, node_id, data, None)
    }

    /// Atomically replace the `data` of an existing node (checked form).
    ///
    /// Enforces namespace isolation (`agent_namespace` must match the tile's namespace)
    /// and the `ModifyOwnTiles` capability, then delegates to `update_node_content_impl`.
    pub fn update_node_content_checked(
        &mut self,
        tile_id: SceneId,
        node_id: SceneId,
        data: NodeData,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        self.update_node_content_impl(tile_id, node_id, data, Some(agent_namespace))
    }

    fn update_node_content_impl(
        &mut self,
        tile_id: SceneId,
        node_id: SceneId,
        mut data: NodeData,
        agent_namespace: Option<&str>,
    ) -> Result<(), ValidationError> {
        // Stage 4: Lease + capability check (when namespace is provided).
        if let Some(ns) = agent_namespace {
            let lease_id = self.get_tile_lease_checked(tile_id, ns)?;
            self.require_active_lease(lease_id)?;
            self.require_capability(lease_id, Capability::ModifyOwnTiles)?;
        } else if !self.tiles.contains_key(&tile_id) {
            return Err(ValidationError::TileNotFound { id: tile_id });
        }

        // Stage 4: Node must exist in the scene graph.
        {
            let existing = self
                .nodes
                .get(&node_id)
                .ok_or(ValidationError::NodeNotFound { id: node_id })?;

            // Stage 4: Node must be reachable from this tile's root.
            let tile = self
                .tiles
                .get(&tile_id)
                .expect("tile_id existence verified above in update_node_content_impl");
            let root = tile
                .root_node
                .ok_or(ValidationError::NodeNotFound { id: node_id })?;
            if !self.is_node_in_subtree(root, node_id) {
                return Err(ValidationError::InvalidField {
                    field: "node_id".into(),
                    reason: format!("node {node_id} does not belong to tile {tile_id}"),
                });
            }

            // Stage 4: Type discriminant must match.
            let type_matches = matches!(
                (&existing.data, &data),
                (NodeData::TextMarkdown(_), NodeData::TextMarkdown(_))
                    | (NodeData::SolidColor(_), NodeData::SolidColor(_))
                    | (NodeData::HitRegion(_), NodeData::HitRegion(_))
                    | (NodeData::StaticImage(_), NodeData::StaticImage(_))
            );
            if !type_matches {
                return Err(ValidationError::InvalidField {
                    field: "data".into(),
                    reason: format!(
                        "cannot change node type: existing node {node_id} has a different variant"
                    ),
                });
            }
        }

        // Content constraints (e.g. markdown byte limit).
        if let Some(err) = validate_text_markdown_node_data(&data) {
            return Err(err);
        }

        // Enforce resource registration for agent-submitted StaticImage content updates.
        //
        // Per spec resource-store/spec.md §Requirement: Resource Upload Before Tile
        // Creation: "Any agent-submitted tile mutation that references a ResourceId not
        // present in the resource store MUST be rejected."
        //
        // This gate closes the bypass where an agent could swap a StaticImageNode to an
        // unregistered resource_id via UpdateNodeContent while passing the add/set_root
        // checks.  Only applied for agent-submitted paths (agent_namespace.is_some()).
        if agent_namespace.is_some() {
            if let NodeData::StaticImage(ref si) = data {
                if !self.registered_resources.contains_key(&si.resource_id) {
                    return Err(ValidationError::ResourceNotFound { id: si.resource_id });
                }
            }
        }

        // Budget re-accounting for StaticImage replacement.
        //
        // Proto ingest always sets `decoded_bytes = 0` on inbound `StaticImageNode`
        // payloads because `decoded_bytes` is runtime-owned metadata that the client
        // must not supply (see `convert.rs`).  If we blindly wrote the incoming zero
        // into the graph the texture-budget tracking in `sum_texture_bytes` /
        // `lease_resource_usage` would under-report actual GPU memory usage after
        // the replacement.
        //
        // Preservation rule:
        //   • Same resource_id AND incoming decoded_bytes == 0 → preserve the
        //     stored decoded_bytes (the image content has not changed; the stored
        //     value is authoritative for budget accounting).
        //   • resource_id changed OR incoming decoded_bytes > 0 → use the incoming
        //     value.  The caller (session server or test) is responsible for
        //     populating decoded_bytes from the resource store when the resource
        //     changes.
        {
            let node = self
                .nodes
                .get_mut(&node_id)
                .expect("node_id existence verified in Stage 4 above");
            if let (NodeData::StaticImage(old_si), NodeData::StaticImage(new_si)) =
                (&node.data, &mut data)
            {
                if new_si.resource_id == old_si.resource_id && new_si.decoded_bytes == 0 {
                    new_si.decoded_bytes = old_si.decoded_bytes;
                }
            }
        }

        // Resource ref-count maintenance for StaticImage content swaps.
        //
        // Extract the old resource_id before re-borrowing mutably, then update
        // ref counts and finally apply the data swap.  The borrow checker requires
        // that the immutable borrow of `node.data` (to read old_id) ends before
        // the mutable borrows of `self` (for dec/inc_resource_ref) begin.
        //
        // This correctly handles:
        //   1. Same resource_id → net zero change; no update needed.
        //   2. Different resource_id → old loses a ref, new gains one.
        let old_resource_id = if let NodeData::StaticImage(ref old_si) = self.nodes[&node_id].data {
            Some(old_si.resource_id)
        } else {
            None
        };
        if let (Some(old_id), NodeData::StaticImage(new_si)) = (old_resource_id, &data) {
            let new_id = new_si.resource_id;
            if old_id != new_id {
                self.dec_resource_ref(&old_id);
                self.inc_resource_ref(new_id);
            }
            // If resource_id is unchanged, ref count is unchanged.
        }

        // Apply the update — replace data in-place, preserving id and children.
        let node = self
            .nodes
            .get_mut(&node_id)
            .expect("node_id existence verified in Stage 4 above");
        node.data = data;
        self.version += 1;
        Ok(())
    }
}

// ─── Helper for TextMarkdownNode content size validation ──────────────────────

/// Validate a TextMarkdownNode's content size.
///
/// Returns `Some(ValidationError)` if the content exceeds `MAX_MARKDOWN_BYTES`.
/// Used by `set_tile_root_impl` when strict content validation is needed.
pub fn validate_text_markdown_node_data(data: &NodeData) -> Option<ValidationError> {
    if let NodeData::TextMarkdown(tm) = data {
        if tm.content.len() > MAX_MARKDOWN_BYTES {
            return Some(ValidationError::InvalidField {
                field: "content".into(),
                reason: format!(
                    "TextMarkdownNode content exceeds {} UTF-8 bytes (got {})",
                    MAX_MARKDOWN_BYTES,
                    tm.content.len()
                ),
            });
        }
    }
    None
}
