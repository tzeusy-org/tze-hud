use super::*;

impl SceneGraph {
    // ─── Queries ─────────────────────────────────────────────────────────

    /// Snapshot the entire scene graph as JSON.
    pub fn snapshot_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deserialize a scene graph from JSON.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Take a deterministic full scene snapshot at the current sequence number.
    ///
    /// Implements RFC 0001 §4.1 — produces a complete, deterministic serialization
    /// of the scene graph. All maps use BTreeMap for deterministic iteration.
    ///
    /// # Checksum
    /// The returned `SceneGraphSnapshot.checksum` is a BLAKE3 hash (hex-encoded) computed
    /// over the canonical JSON of the snapshot with the checksum field set to `""`.
    /// Snapshot fixtures verify this value against the canonical content
    /// after deserialization.
    ///
    /// # Clock arguments
    /// `wall_us` is UTC wall-clock microseconds since epoch (u64). `mono_us` is
    /// monotonic microseconds since process start.
    ///
    /// # v1 Constraints
    /// - Resources are referenced by ResourceId only; no blob data is included.
    /// - effective_geometry is NOT included (post-v1 per spec line 360).
    /// - Incremental diff is NOT available (snapshot-only reconnect in v1).
    pub fn take_snapshot(&self, wall_us: u64, mono_us: u64) -> SceneGraphSnapshot {
        // Tabs: keyed by display_order for deterministic ordering.
        let tabs: std::collections::BTreeMap<u32, Tab> = self
            .tabs
            .values()
            .map(|t| (t.display_order, t.clone()))
            .collect();

        // Tiles: keyed by SceneId (BTreeMap — SceneId implements Ord).
        let tiles: std::collections::BTreeMap<SceneId, Tile> =
            self.tiles.iter().map(|(k, v)| (*k, v.clone())).collect();

        // Nodes: keyed by SceneId.
        let nodes: std::collections::BTreeMap<SceneId, Node> =
            self.nodes.iter().map(|(k, v)| (*k, v.clone())).collect();

        // Portal surfaces: keyed by host tile SceneId (BTreeMap — deterministic).
        //
        // Overlay state (RuntimeOverlayState::portal_surfaces, #[serde(skip)]) that
        // must be recoverable on reconnect (hud-ruynm). Cloned verbatim: a part
        // whose backing node was nulled by revalidate_portal_surface_part_nodes is
        // serialized with node = None, never fabricated. Visibility mirrors `tiles`
        // — each surface is keyed by its host tile, so a namespace-filtered view of
        // the snapshot keeps exactly the surfaces on the tiles it retains.
        let portal_surfaces: std::collections::BTreeMap<SceneId, PortalSurface> = self
            .overlay
            .portal_surfaces
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();

        // Zone registry: BTreeMap for both zone_types and active_publications.
        let zone_types: std::collections::BTreeMap<String, ZoneDefinition> = self
            .zone_registry
            .zones
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Zone instances: intentionally empty in v1.
        // In v1 the zone_registry does not store ZoneInstance directly —
        // instance binding is implicit (one per tab per zone type when a zone is loaded).
        // Consumers of this snapshot MUST NOT rely on zone_instances being populated;
        // any instance bindings must be derived from zone_types and current tab/node state.
        // Post-v1: explicit ZoneInstance tracking will populate this field.
        let zone_instances: Vec<ZoneInstance> = Vec::new();

        // Active publications: BTreeMap keyed by zone name; each Vec is already
        // ordered by insertion (policy-enforced). Sort each Vec with a total order
        // to guarantee determinism: published_at_wall_us → publisher_namespace → merge_key.
        // The merge_key tie-breaker ensures records that share a timestamp and namespace
        // (e.g., MergeByKey records) are still ordered deterministically.
        let active_publications: std::collections::BTreeMap<String, Vec<ZonePublishRecord>> = self
            .zone_registry
            .active_publishes
            .iter()
            .map(|(zone_name, records)| {
                let mut sorted = records.clone();
                sorted.sort_by(|a, b| {
                    a.published_at_wall_us
                        .cmp(&b.published_at_wall_us)
                        .then_with(|| a.publisher_namespace.cmp(&b.publisher_namespace))
                        .then_with(|| a.merge_key.cmp(&b.merge_key))
                });
                (zone_name.clone(), sorted)
            })
            .collect();

        let zone_registry = SceneGraphZoneRegistry {
            zone_types,
            zone_instances,
            active_publications,
        };

        // Widget registry: BTreeMap for deterministic serialization.
        let widget_types: std::collections::BTreeMap<String, WidgetDefinition> = self
            .widget_registry
            .definitions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Widget instances: sorted by instance_name for determinism.
        let mut widget_instances: Vec<WidgetInstance> =
            self.widget_registry.instances.values().cloned().collect();
        widget_instances.sort_by(|a, b| a.instance_name.cmp(&b.instance_name));

        // Widget active publications: BTreeMap keyed by instance_name; sort each Vec
        // by published_at_wall_us → publisher_namespace → merge_key for determinism.
        let widget_active_publications: std::collections::BTreeMap<
            String,
            Vec<WidgetPublishRecord>,
        > = self
            .widget_registry
            .active_publishes
            .iter()
            .map(|(name, records)| {
                let mut sorted = records.clone();
                sorted.sort_by(|a, b| {
                    a.published_at_wall_us
                        .cmp(&b.published_at_wall_us)
                        .then_with(|| a.publisher_namespace.cmp(&b.publisher_namespace))
                        .then_with(|| a.merge_key.cmp(&b.merge_key))
                });
                (name.clone(), sorted)
            })
            .collect();

        let widget_registry = SceneGraphWidgetRegistry {
            widget_types,
            widget_instances,
            active_publications: widget_active_publications,
        };

        // Build the snapshot with a placeholder checksum first.
        let mut snapshot = SceneGraphSnapshot {
            sequence: self.sequence_number,
            snapshot_wall_us: wall_us,
            snapshot_mono_us: mono_us,
            tabs,
            tiles,
            nodes,
            portal_surfaces,
            zone_registry,
            widget_registry,
            active_tab: self.active_tab,
            display_area: self.display_area,
            checksum: String::new(),
        };

        // Compute and assign the checksum over the canonical content.
        snapshot.checksum = snapshot.compute_checksum();
        snapshot
    }

    /// Count total nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Count total tiles in the graph.
    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }
}
