//! Mutation batch operations for atomic scene changes.
//!
//! # Transaction Validation Pipeline (RFC 0001 §3.2, §3.3)
//!
//! Every [`MutationBatch`] passes through five ordered validation stages before
//! any mutation is applied to the live scene graph:
//!
//! | Stage | Check | Early exit |
//! |-------|-------|-----------|
//! | 1 | **Lease check** — lease exists and is Active | Yes |
//! | 2 | **Budget check** — batch fits within lease resource budget | Yes |
//! | 3 | **Bounds check** — geometry is valid (positive dimensions, finite values) | Per-mutation |
//! | 4 | **Type check** — mutation references are consistent; mutation type is legal | Per-mutation |
//! | 5 | **Invariant check** — post-mutation simulation: no cycles, no z-order conflicts | Post-apply |
//!
//! Stages run in this order. A failure at any stage produces a [`BatchRejected`] and
//! no mutations are applied (all-or-nothing).
//!
//! # Batch size limit (RFC 0001 §3.1)
//!
//! The maximum batch size is [`MAX_BATCH_SIZE`] (1000 mutations).  Batches
//! exceeding this limit are rejected with [`ValidationError::BatchSizeExceeded`]
//! before any stage runs.
//!
//! # Agent namespace (RFC 0001 §3.1)
//!
//! `agent_namespace` MUST be derived from the authenticated session context.
//! It is carried in the batch struct for in-process callers but the gRPC layer
//! MUST overwrite it from the authenticated principal before calling
//! [`SceneGraph::apply_batch`].

use crate::graph::SceneGraph;
use crate::timing::domains::WallUs;
use crate::types::*;
#[cfg(test)]
use crate::validation::ValidationErrorCode;
use crate::validation::{BatchRejected, ValidationError};
use serde::{Deserialize, Serialize};

/// Maximum number of mutations in a single batch (RFC 0001 §3.1).
pub const MAX_BATCH_SIZE: usize = 1_000;

/// An atomic batch of scene mutations from an agent.
///
/// # Wire contract
/// - `batch_id` is a UUIDv7 `SceneId`.
/// - `agent_namespace` is filled by the runtime from the authenticated session;
///   client-supplied values MUST be ignored by the gRPC layer.
/// - `mutations` are applied in order, atomically (all-or-nothing).
/// - Optional `timing_hints` carry `present_at_wall_us` and `expires_at_wall_us` (as [`WallUs`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MutationBatch {
    pub batch_id: SceneId,
    pub agent_namespace: String,
    pub mutations: Vec<SceneMutation>,
    /// Optional timing hints from the agent.
    pub timing_hints: Option<BatchTimingHints>,
    /// Lease ID for this batch. Required for lease/budget validation.
    /// If absent, lease validation is skipped (use with care in tests only).
    pub lease_id: Option<SceneId>,
}

/// Optional timing hints attached to a [`MutationBatch`] (RFC 0005).
///
/// Named `BatchTimingHints` to distinguish it from [`crate::timing::TimingHints`],
/// which is the per-node/per-payload scheduling struct used by the compositor.
/// Fields use the [`WallUs`] newtype for clock-domain safety.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchTimingHints {
    /// Wall-clock time at which the batch should be presented.
    pub present_at_wall_us: Option<WallUs>,
    /// Wall-clock time at which the batch expires.
    pub expires_at_wall_us: Option<WallUs>,
}

/// Individual scene mutations (v1 set per RFC 0001 §3.1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SceneMutation {
    // ── Tab mutations ─────────────────────────────────────────────────────
    // NOTE: Tab mutations require the `manage_tabs` capability per RFC 0001
    // §2.2, §3.3. However, `SceneMutation` variants do not carry a `lease_id`
    // field, so capability enforcement at the batch-apply layer must be done
    // by the transport/session layer (gRPC handler) before calling
    // `apply_batch`. The scene graph's `create_tab_with_lease` /
    // `delete_tab_with_lease` / etc. checked variants are available for
    // direct callers that have a lease in scope.
    //
    // Tab mutations in `apply_single_mutation` call the unchecked graph
    // methods; the gRPC layer is responsible for verifying `manage_tabs`
    // before dispatching the batch.
    /// Create a new tab. RFC 0001 §2.2.
    CreateTab { name: String, display_order: u32 },
    /// Delete a tab and all its tiles. RFC 0001 §2.2.
    DeleteTab { tab_id: SceneId },
    /// Rename a tab. RFC 0001 §2.2.
    RenameTab { tab_id: SceneId, new_name: String },
    /// Change the display_order of a tab. RFC 0001 §2.2.
    ReorderTab { tab_id: SceneId, new_order: u32 },
    /// Switch the active tab. RFC 0001 §2.2.
    SwitchActiveTab { tab_id: SceneId },
    // ── Tile mutations (require create_tiles / modify_own_tiles) ──────────
    /// Create a new tile. Requires `create_tiles` + `modify_own_tiles`. RFC 0001 §2.3.
    CreateTile {
        tab_id: SceneId,
        namespace: String,
        lease_id: SceneId,
        bounds: Rect,
        z_order: u32,
    },
    /// Update tile bounds. RFC 0001 §2.3.
    UpdateTileBounds { tile_id: SceneId, bounds: Rect },
    /// Update tile z-order. RFC 0001 §2.3.
    UpdateTileZOrder { tile_id: SceneId, z_order: u32 },
    /// Update tile opacity (must be in [0.0, 1.0]). RFC 0001 §2.3.
    UpdateTileOpacity { tile_id: SceneId, opacity: f32 },
    /// Update tile input mode. RFC 0001 §2.3.
    UpdateTileInputMode {
        tile_id: SceneId,
        input_mode: InputMode,
    },
    /// Update tile expiry timestamp. RFC 0001 §2.3.
    UpdateTileExpiry {
        tile_id: SceneId,
        expires_at: Option<u64>,
    },
    /// Delete a tile and all its nodes. RFC 0001 §2.3.
    DeleteTile { tile_id: SceneId },
    // ── Node mutations ────────────────────────────────────────────────────
    SetTileRoot {
        tile_id: SceneId,
        /// Root node of the tile's content subtree. `node.children` reference the
        /// SceneIds materialized from `descendants`.
        node: Node,
        /// Inline descendants of `node` (any depth), root EXCLUDED, in a flat
        /// list (hud-ga4md). Materialized atomically with the root as ONE
        /// mutation so a multi-node portal body (transcript + head-anchored
        /// composer + INPUT band) rides a single coalescible StateStream update
        /// instead of a per-node `AddNode` fan-out that would flip the batch
        /// Transactional and break republish latest-wins coalescing (hud-mzk74).
        /// Empty = a flat single-node root (pre-children behavior).
        descendants: Vec<Node>,
    },
    AddNode {
        tile_id: SceneId,
        parent_id: Option<SceneId>,
        node: Node,
    },
    /// Atomically replace the content of an existing node.
    ///
    /// The node must already exist in the scene graph, belong to `tile_id`,
    /// and the replacement `data` must match the node's current type
    /// (e.g. `TextMarkdown` can only be updated with `TextMarkdown` data).
    /// This allows periodic in-place content updates (e.g. live text refresh)
    /// without the overhead of RemoveNode + AddNode or SetTileRoot.
    ///
    /// Validation (Stage 4):
    /// - `tile_id` must exist and be owned by the calling agent namespace.
    /// - `node_id` must exist in the scene graph.
    /// - `node_id` must be reachable from `tile_id`'s root node.
    /// - The discriminant of `data` must match the existing node's discriminant.
    /// - Content constraints (e.g. markdown byte limit) are re-enforced.
    UpdateNodeContent {
        tile_id: SceneId,
        node_id: SceneId,
        data: NodeData,
    },
    // ── Zone mutations ────────────────────────────────────────────────────
    /// Publish content to a zone.
    PublishToZone {
        zone_name: String,
        content: ZoneContent,
        publish_token: ZonePublishToken,
        /// For MergeByKey contention: the key under which content is stored.
        merge_key: Option<String>,
        /// Optional wall-clock expiry timestamp (microseconds since epoch).
        /// When set, the runtime clears this publication at or before expiry.
        expires_at_wall_us: Option<u64>,
        /// Optional opaque content classification tag (e.g., "public", "pii").
        content_classification: Option<String>,
        /// Byte-offset breakpoints for StreamText word-by-word reveal.
        ///
        /// Only meaningful when `content` is `ZoneContent::StreamText`.
        /// Empty = reveal full text immediately.
        /// Per spec §Subtitle Streaming Word-by-Word Reveal.
        #[serde(default)]
        breakpoints: Vec<u64>,
    },
    /// Clear all publications by this agent in the specified zone.
    ///
    /// Per spec: "ClearZone clears all publications by the agent in the specified zone."
    ClearZone {
        zone_name: String,
        publish_token: ZonePublishToken,
    },
    /// Clear all publications by this agent on the specified widget instance.
    ///
    /// Mirrors ClearZone semantics for widgets: removes only the calling agent's
    /// publications. If no publications exist for the publisher, this is a no-op
    /// (but still succeeds). When no publishers remain the widget reverts to its
    /// default parameter values.
    ClearWidget {
        /// Widget instance name (addressing key).
        widget_name: String,
        /// Optional disambiguation when multiple instances share the same name.
        instance_id: Option<String>,
    },
    // ── Scroll mutations (require ModifyOwnTiles) ──────────────────────
    /// Register or replace local-first scroll config for a tile.
    /// Enables adapter-driven scroll via `SetScrollOffset`.
    /// Renderer/coalescer/clamp plumbing shipped in hud-w5ih (PR #489).
    RegisterTileScroll {
        tile_id: SceneId,
        scrollable_x: bool,
        scrollable_y: bool,
        content_width: Option<f32>,
        content_height: Option<f32>,
    },
    /// Set the tile-local scroll offset. Clamped by the tile's scroll config
    /// (content_width / content_height). Requires a prior `RegisterTileScroll`.
    SetScrollOffset {
        tile_id: SceneId,
        offset_x: f32,
        offset_y: f32,
    },
    /// Set (or clear) the lifecycle-affordance accent for a tile. Coalescible
    /// StateStream tile-update (RFC 0005 §3.3 MutationBatch, client→server §3.1):
    /// reflects the portal's lifecycle
    /// state and is updated on transition, never re-added per content republish
    /// (hud-m48i0). `accent = None` clears it (e.g. lifecycle redacted/absent).
    SetTileLifecycleAccent {
        tile_id: SceneId,
        accent: Option<LifecycleAccent>,
    },
    /// Set (or clear) the ambient unread-output count a tile's jump-to-latest
    /// pill MAY carry as a badge (hud-g1ena.3). Coalescible StateStream
    /// tile-update (RFC 0005 §3.3), mirroring `SetTileLifecycleAccent`: it
    /// reflects runtime-owned unread state, is refreshed each portal drain, and
    /// is stored as overlay state keyed by `tile_id` so it survives transcript
    /// republishes. `count = 0` clears the badge. This is the bridged-transport
    /// counterpart of the in-process driver's direct `set_tile_unread_count`
    /// call, giving a bridged portal's pill the same badge (hud-hwk2m).
    SetTileUnreadCount { tile_id: SceneId, count: usize },
    /// Set (or clear) the composer interaction hit region an interaction-enabled
    /// portal exposes over its host tile (hud-iofav). Coalescible StateStream
    /// tile-update (RFC 0005 §3.3), mirroring `SetTileLifecycleAccent`: the spec is
    /// stored as overlay state keyed by `tile_id` and the scene derives a
    /// hit-region child of the tile root from it, re-attaching that node after each
    /// transcript republish — so an interaction-enabled portal's streaming publish
    /// never rides a per-republish `AddNode` (which would flip the batch
    /// Transactional, hud-mzk74). `region = None` clears it (interaction disabled).
    SetTileComposerInteraction {
        tile_id: SceneId,
        region: Option<HitRegionNode>,
    },
    // ── Portal surface (RFC 0013 §7.2 promotion; hud-tc153) ───────────────
    /// Declare or replace the first-class portal surface descriptor over a tile.
    ///
    /// Transactional (RFC 0005 §3.1): establishes the promoted surface — its
    /// identity, lifecycle/display state, and the eight named parts — as one
    /// governed object in place of the ad-hoc six-tile raw assembly. The
    /// descriptor is stored as runtime overlay state keyed by `tile_id` so it
    /// survives transcript republishes (`SetTileRoot`/`PublishToTile`). Requires
    /// `modify_own_tiles`; each materialized part node must belong to `tile_id`.
    SetPortalSurface {
        tile_id: SceneId,
        surface: PortalSurface,
    },
    /// Patch the lifecycle and/or display state of an existing portal surface.
    ///
    /// Coalescible StateStream tile-update (RFC 0005 §3.3): a `None` field is
    /// left unchanged; the surface's parts and identity are untouched, so a
    /// frequent lifecycle/collapse transition never re-sends the whole surface
    /// (mirrors the `SetTileLifecycleAccent` coalescing rationale, hud-mzk74).
    UpdatePortalSurfaceState {
        tile_id: SceneId,
        lifecycle: Option<PortalLifecycleState>,
        display_state: Option<PortalDisplayState>,
    },
}

impl SceneMutation {
    /// Return the human-readable type name for structured error responses.
    pub fn type_name(&self) -> &'static str {
        match self {
            SceneMutation::CreateTab { .. } => "CreateTab",
            SceneMutation::DeleteTab { .. } => "DeleteTab",
            SceneMutation::RenameTab { .. } => "RenameTab",
            SceneMutation::ReorderTab { .. } => "ReorderTab",
            SceneMutation::SwitchActiveTab { .. } => "SwitchActiveTab",
            SceneMutation::CreateTile { .. } => "CreateTile",
            SceneMutation::UpdateTileBounds { .. } => "UpdateTileBounds",
            SceneMutation::UpdateTileZOrder { .. } => "UpdateTileZOrder",
            SceneMutation::UpdateTileOpacity { .. } => "UpdateTileOpacity",
            SceneMutation::UpdateTileInputMode { .. } => "UpdateTileInputMode",
            SceneMutation::UpdateTileExpiry { .. } => "UpdateTileExpiry",
            SceneMutation::DeleteTile { .. } => "DeleteTile",
            SceneMutation::SetTileRoot { .. } => "SetTileRoot",
            SceneMutation::AddNode { .. } => "AddNode",
            SceneMutation::UpdateNodeContent { .. } => "UpdateNodeContent",
            SceneMutation::PublishToZone { .. } => "PublishToZone",
            SceneMutation::ClearZone { .. } => "ClearZone",
            SceneMutation::ClearWidget { .. } => "ClearWidget",
            SceneMutation::RegisterTileScroll { .. } => "RegisterTileScroll",
            SceneMutation::SetScrollOffset { .. } => "SetScrollOffset",
            SceneMutation::SetTileLifecycleAccent { .. } => "SetTileLifecycleAccent",
            SceneMutation::SetTileUnreadCount { .. } => "SetTileUnreadCount",
            SceneMutation::SetTileComposerInteraction { .. } => "SetTileComposerInteraction",
            SceneMutation::SetPortalSurface { .. } => "SetPortalSurface",
            SceneMutation::UpdatePortalSurfaceState { .. } => "UpdatePortalSurfaceState",
        }
    }
}

/// Result of applying a mutation batch.
#[derive(Clone, Debug)]
pub struct MutationResult {
    pub batch_id: SceneId,
    pub applied: bool,
    pub created_ids: Vec<SceneId>,
    pub error: Option<ValidationError>,
    /// Structured rejection response (RFC 0001 §3.4). Present when `applied == false`.
    pub rejection: Option<BatchRejected>,
    /// True if the lease is at the soft budget warning threshold (80%).
    /// The batch was still applied, but the caller should notify the agent.
    pub budget_warning: bool,
    /// Monotonically increasing sequence number assigned when this batch was committed.
    /// `None` if `applied == false`.
    pub sequence_number: Option<u64>,
}

impl MutationResult {
    fn rejected_with_error(
        batch_id: SceneId,
        rejection: BatchRejected,
        error: ValidationError,
    ) -> Self {
        Self {
            batch_id,
            applied: false,
            created_ids: vec![],
            error: Some(error),
            rejection: Some(rejection),
            budget_warning: false,
            sequence_number: None,
        }
    }
}

impl SceneGraph {
    /// Apply a mutation batch atomically per the five-stage validation pipeline.
    ///
    /// # Pipeline stages (RFC 0001 §3.2)
    ///
    /// 1. **Batch size check** — reject if > [`MAX_BATCH_SIZE`] mutations.
    /// 2. **Stage 1: Lease check** — all referenced leases must be Active.
    ///    Uses `batch.lease_id` if set, else discovers lease_ids from `CreateTile`
    ///    mutations and tile lookups. Expired lease is caught here before budget.
    /// 3. **Stage 2: Budget check** — projected resource usage fits within budget.
    /// 4. **Stage 3: Bounds check** — bounds have positive width/height, finite coords.
    /// 5. **Stage 4: Type check** — referenced tabs/tiles/nodes exist.
    /// 6. **Stage 5: Invariant check (post-mutation simulation)** — apply to a clone
    ///    and verify no cycles, no z-order conflicts, no broken internal references.
    ///
    /// On any failure the live graph is untouched. The returned [`MutationResult`]
    /// carries a structured [`BatchRejected`] with per-mutation diagnostics.
    pub fn apply_batch(&mut self, batch: &MutationBatch) -> MutationResult {
        // ── Batch size limit ───────────────────────────────────────────────
        if batch.mutations.len() > MAX_BATCH_SIZE {
            let err = ValidationError::BatchSizeExceeded {
                max: MAX_BATCH_SIZE,
                got: batch.mutations.len(),
            };
            let rejection = BatchRejected::batch_level(batch.batch_id, "batch", &err);
            return MutationResult::rejected_with_error(batch.batch_id, rejection, err);
        }

        // ── Stage 1: Lease check ──────────────────────────────────────────
        // Collect all lease IDs referenced by this batch.
        let mut lease_ids: Vec<SceneId> = Vec::new();

        // Prefer the explicit batch-level lease_id.
        if let Some(lid) = batch.lease_id {
            lease_ids.push(lid);
        }

        // Also harvest any lease IDs embedded in CreateTile mutations.
        for mutation in &batch.mutations {
            if let SceneMutation::CreateTile { lease_id, .. } = mutation {
                if !lease_ids.contains(lease_id) {
                    lease_ids.push(*lease_id);
                }
            }
        }

        // Deduplicate
        lease_ids.sort();
        lease_ids.dedup();

        // Validate batch.lease_id (and any other batch-level lease IDs collected
        // above) for existence and Active state. This is a batch-level check: a
        // nonexistent or inactive batch.lease_id must be rejected before Stage 2
        // budget checks even if no individual mutation embeds that lease_id
        // (e.g., when the batch contains only tile-targeting mutations).
        //
        // Only leases that came from batch.lease_id are validated here; leases
        // embedded in individual CreateTile mutations are validated in the
        // per-mutation loop below (with per-mutation attribution).
        if let Some(lid) = batch.lease_id {
            if let Some(lease) = self.leases.get(&lid) {
                if !lease.is_mutations_allowed() {
                    let err = if lease.is_expired(self.now_millis()) {
                        ValidationError::LeaseExpired { id: lid }
                    } else {
                        ValidationError::InvalidField {
                            field: "lease_state".into(),
                            reason: format!(
                                "lease {} is in {:?} state; mutations require Active state",
                                lid, lease.state,
                            ),
                        }
                    };
                    let rejection = BatchRejected::batch_level(batch.batch_id, "batch", &err);
                    return MutationResult::rejected_with_error(batch.batch_id, rejection, err);
                }
            } else {
                let err = ValidationError::LeaseNotFound { id: lid };
                let rejection = BatchRejected::batch_level(batch.batch_id, "batch", &err);
                return MutationResult::rejected_with_error(batch.batch_id, rejection, err);
            }
        }

        // Check each mutation's lease: must exist and be Active.
        // lease_id_for_mutation now also derives lease from tile for
        // tile-targeting mutations (UpdateTileBounds, DeleteTile, SetTileRoot,
        // AddNode, and related variants) via graph lookup.
        for (idx, mutation) in batch.mutations.iter().enumerate() {
            let maybe_lease_id = Self::lease_id_for_mutation(mutation, &self.tiles);
            if let Some(lease_id) = maybe_lease_id {
                if let Some(lease) = self.leases.get(&lease_id) {
                    if !lease.is_mutations_allowed() {
                        // Stage 1 failure: lease is not Active
                        let err = if lease.is_expired(self.now_millis()) {
                            ValidationError::LeaseExpired { id: lease_id }
                        } else {
                            ValidationError::InvalidField {
                                field: "lease_state".into(),
                                reason: format!(
                                    "lease {} is in {:?} state; mutations require Active state",
                                    lease_id, lease.state,
                                ),
                            }
                        };
                        let rejection =
                            BatchRejected::single(batch.batch_id, idx, mutation.type_name(), &err);
                        return MutationResult::rejected_with_error(batch.batch_id, rejection, err);
                    }
                } else {
                    // Only report LeaseNotFound for mutations that embed their own
                    // lease_id (CreateTile). For tile-targeting mutations whose lease
                    // was derived from the tile, the tile not having a valid lease is
                    // a transient state that should be reported as LeaseNotFound.
                    let err = ValidationError::LeaseNotFound { id: lease_id };
                    let rejection =
                        BatchRejected::single(batch.batch_id, idx, mutation.type_name(), &err);
                    return MutationResult::rejected_with_error(batch.batch_id, rejection, err);
                }
            }
        }

        // ── Stage 2: Budget check ─────────────────────────────────────────
        let mut budget_warning = false;
        for lid in &lease_ids {
            if let Err(budget_err) = self.check_budget(lid, batch) {
                let err = ValidationError::BudgetExceeded {
                    resource: format!("{budget_err}"),
                };
                let rejection = BatchRejected::batch_level(batch.batch_id, "batch", &err);
                return MutationResult::rejected_with_error(batch.batch_id, rejection, err);
            }
            if self.is_lease_budget_warning(lid) {
                budget_warning = true;
            }
        }

        // ── Stages 3 + 4 + 5: Apply to a snapshot, collect per-mutation errors ──
        // Clone the scene for rollback and for the post-mutation invariant check.
        let snapshot = self.clone();
        let mut created_ids = Vec::new();

        // Snapshot registered resource IDs before mutation application.
        //
        // Within a batch, `SetTileRoot` removes the old node tree (decrementing
        // resource ref counts, potentially removing the resource from the map),
        // while a subsequent `AddNode` may re-add a StaticImageNode referencing
        // the same resource.  Without this guard the `AddNode` resource check
        // would fail because the resource was transiently de-registered.
        //
        // After all mutations are applied, any resource that was in the snapshot
        // but was removed during mutation application and still has no references
        // is cleaned up below.
        let pre_batch_resources: std::collections::HashSet<crate::types::ResourceId> =
            self.registered_resources.keys().copied().collect();

        for (idx, mutation) in batch.mutations.iter().enumerate() {
            // Before each mutation, ensure resources that were registered at
            // batch start remain visible to the resource-registration gate.
            for rid in &pre_batch_resources {
                self.registered_resources.entry(*rid).or_insert(0);
            }

            // Stage 3: Bounds check (in-line in apply_single_mutation via bounds validation)
            // Stage 4: Type check (in-line — references validated by apply_single_mutation)
            match self.apply_single_mutation(mutation, &batch.agent_namespace) {
                Ok(ids) => created_ids.extend(ids),
                Err(e) => {
                    // Rollback to snapshot
                    *self = snapshot;
                    let rejection =
                        BatchRejected::single(batch.batch_id, idx, mutation.type_name(), &e);
                    return MutationResult::rejected_with_error(batch.batch_id, rejection, e);
                }
            }
        }

        // Clean up: remove resources that were transiently kept alive by the
        // pre-batch guard, ended the batch with ref count 0, AND were actually
        // removed during this batch (i.e., had count > 0 at batch start).
        // Resources that started at count 0 (registered but unused) must survive
        // across batches — they may be referenced by a later batch.
        for rid in &pre_batch_resources {
            if let Some(&count) = self.registered_resources.get(rid) {
                if count == 0 {
                    // Only remove if this resource had refs at batch start (meaning
                    // it was transiently freed during this batch, not pre-existing at 0).
                    if let Some(&pre_count) = snapshot.registered_resources.get(rid) {
                        if pre_count > 0 {
                            self.registered_resources.remove(rid);
                        }
                    }
                }
            }
        }

        // ── Stage 5: Post-mutation invariant check ────────────────────────
        if let Err(e) = self.check_post_mutation_invariants(batch) {
            // Rollback: the invariant check found a violation
            *self = snapshot;
            let rejection = BatchRejected::batch_level(batch.batch_id, "batch", &e);
            return MutationResult::rejected_with_error(batch.batch_id, rejection, e);
        }

        // ── Commit ────────────────────────────────────────────────────────
        // Assign a monotonically increasing sequence number.
        let seq = self.next_sequence_number();

        // Re-check budget warning after application (usage may have changed).
        for lid in &lease_ids {
            if self.is_lease_budget_warning(lid) {
                budget_warning = true;
            }
        }

        // Record this batch for batch-correlated present acknowledgment (hud-91uu6).
        // The commit above mutated the live graph, so the next composited frame
        // carries this batch; the render loop drains and stamps it with the
        // present wall-clock to emit a FramePresented event.
        self.record_present_ack_batch(batch.batch_id);

        MutationResult {
            batch_id: batch.batch_id,
            applied: true,
            created_ids,
            error: None,
            rejection: None,
            budget_warning,
            sequence_number: Some(seq),
        }
    }

    /// Extract the lease_id for a mutation, if applicable.
    ///
    /// For `CreateTile` the lease_id is embedded in the mutation directly.
    /// For tile-targeting mutations (`UpdateTileBounds`, `UpdateTileZOrder`,
    /// `UpdateTileOpacity`, `UpdateTileInputMode`, `UpdateTileExpiry`,
    /// `DeleteTile`, `SetTileRoot`, `AddNode`, `UpdateNodeContent`) the lease is
    /// derived from the tile in the graph. This enables Stage 1 to catch
    /// expired/revoked leases for all mutation types, not just `CreateTile`.
    fn lease_id_for_mutation(
        mutation: &SceneMutation,
        tiles: &std::collections::HashMap<SceneId, Tile>,
    ) -> Option<SceneId> {
        match mutation {
            SceneMutation::CreateTile { lease_id, .. } => Some(*lease_id),
            // Tile-targeting mutations: derive lease from the tile's recorded lease_id.
            SceneMutation::UpdateTileBounds { tile_id, .. }
            | SceneMutation::UpdateTileZOrder { tile_id, .. }
            | SceneMutation::UpdateTileOpacity { tile_id, .. }
            | SceneMutation::UpdateTileInputMode { tile_id, .. }
            | SceneMutation::UpdateTileExpiry { tile_id, .. }
            | SceneMutation::DeleteTile { tile_id }
            | SceneMutation::SetTileRoot { tile_id, .. }
            | SceneMutation::AddNode { tile_id, .. }
            | SceneMutation::UpdateNodeContent { tile_id, .. }
            | SceneMutation::RegisterTileScroll { tile_id, .. }
            | SceneMutation::SetScrollOffset { tile_id, .. }
            | SceneMutation::SetTileLifecycleAccent { tile_id, .. }
            | SceneMutation::SetTileUnreadCount { tile_id, .. }
            | SceneMutation::SetPortalSurface { tile_id, .. }
            | SceneMutation::UpdatePortalSurfaceState { tile_id, .. } => {
                tiles.get(tile_id).map(|t| t.lease_id)
            }
            // Tab and zone mutations: no per-mutation lease check at Stage 1.
            _ => None,
        }
    }

    /// Check post-mutation invariants on the (already mutated) working graph.
    ///
    /// Stage 5: verifies:
    /// 1. No cycles in node trees.
    /// 2. No exclusive z-order conflicts among non-passthrough tiles on the same tab.
    fn check_post_mutation_invariants(
        &self,
        _batch: &MutationBatch,
    ) -> Result<(), ValidationError> {
        // 5a: Cycle detection in node trees
        // Walk each tile's root node tree and ensure no node appears twice.
        for tile in self.tiles.values() {
            if let Some(root_id) = tile.root_node {
                let mut visited = std::collections::HashSet::new();
                if let Err(cycle_node) = self.detect_cycle(root_id, &mut visited) {
                    return Err(ValidationError::CycleDetected {
                        node_id: cycle_node,
                    });
                }
            }
        }

        // 5b: Z-order conflict detection
        // Group tiles by tab. Within each tab, detect non-passthrough tiles that share
        // a z_order AND have overlapping bounds.
        let mut tab_tiles: std::collections::HashMap<SceneId, Vec<&Tile>> =
            std::collections::HashMap::new();
        for tile in self.tiles.values() {
            tab_tiles.entry(tile.tab_id).or_default().push(tile);
        }

        for tiles in tab_tiles.values() {
            // O(n²) is fine for the max tile count (64 per lease, 64 leases = 4096 max,
            // but in practice batches are small). If this becomes a bottleneck we can
            // bucket by z_order first.
            for i in 0..tiles.len() {
                for j in (i + 1)..tiles.len() {
                    let a = tiles[i];
                    let b = tiles[j];
                    if a.z_order == b.z_order
                        && a.input_mode != InputMode::Passthrough
                        && b.input_mode != InputMode::Passthrough
                        && a.bounds.intersects(&b.bounds)
                    {
                        return Err(ValidationError::ZOrderConflict {
                            tile_a: a.id,
                            tile_b: b.id,
                            z_order: a.z_order,
                        });
                    }
                }
            }
        }

        Ok(())
    }

    /// DFS cycle detection for a node subtree.
    ///
    /// Returns `Ok(())` if the subtree is acyclic, or `Err(node_id)` identifying the
    /// node that creates the cycle.
    ///
    /// # Algorithm
    ///
    /// `visited` tracks the current DFS path (nodes on the active recursion stack).
    /// A node is removed from `visited` when the recursion backtracks from it, so
    /// shared child nodes (valid in a DAG) are not incorrectly flagged as cycles.
    /// Only true back-edges (node encountered while still on the active path) are
    /// rejected as cycles.
    fn detect_cycle(
        &self,
        node_id: SceneId,
        visited: &mut std::collections::HashSet<SceneId>,
    ) -> Result<(), SceneId> {
        if !visited.insert(node_id) {
            // node_id is already on the active DFS path → back-edge → cycle
            return Err(node_id);
        }
        if let Some(node) = self.nodes.get(&node_id) {
            for &child_id in &node.children {
                self.detect_cycle(child_id, visited)?;
            }
        }
        // Backtrack: remove from path so sibling branches can share this node
        // without being falsely flagged as cycles.
        visited.remove(&node_id);
        Ok(())
    }

    fn apply_single_mutation(
        &mut self,
        mutation: &SceneMutation,
        namespace: &str,
    ) -> Result<Vec<SceneId>, ValidationError> {
        match mutation {
            // ── Tab mutations ─────────────────────────────────────────────────
            SceneMutation::CreateTab {
                name,
                display_order,
            } => {
                let id = self.create_tab(name, *display_order)?;
                Ok(vec![id])
            }
            SceneMutation::DeleteTab { tab_id } => {
                self.delete_tab(*tab_id)?;
                Ok(vec![])
            }
            SceneMutation::RenameTab { tab_id, new_name } => {
                self.rename_tab(*tab_id, new_name)?;
                Ok(vec![])
            }
            SceneMutation::ReorderTab { tab_id, new_order } => {
                self.reorder_tab(*tab_id, *new_order)?;
                Ok(vec![])
            }
            SceneMutation::SwitchActiveTab { tab_id } => {
                self.switch_active_tab(*tab_id)?;
                Ok(vec![])
            }
            // ── Tile mutations ────────────────────────────────────────────────
            SceneMutation::CreateTile {
                tab_id,
                namespace,
                lease_id,
                bounds,
                z_order,
            } => {
                let id = self.create_tile(*tab_id, namespace, *lease_id, *bounds, *z_order)?;
                Ok(vec![id])
            }
            SceneMutation::UpdateTileBounds { tile_id, bounds } => {
                // Route through the checked method to enforce namespace isolation,
                // lease/capability checks, and the within-display-area invariant.
                self.update_tile_bounds(*tile_id, *bounds, namespace)?;
                Ok(vec![])
            }
            SceneMutation::UpdateTileZOrder { tile_id, z_order } => {
                self.update_tile_z_order(*tile_id, *z_order, namespace)?;
                Ok(vec![])
            }
            SceneMutation::UpdateTileOpacity { tile_id, opacity } => {
                self.update_tile_opacity(*tile_id, *opacity, namespace)?;
                Ok(vec![])
            }
            SceneMutation::UpdateTileInputMode {
                tile_id,
                input_mode,
            } => {
                self.update_tile_input_mode(*tile_id, *input_mode, namespace)?;
                Ok(vec![])
            }
            SceneMutation::UpdateTileExpiry {
                tile_id,
                expires_at,
            } => {
                self.update_tile_expiry(*tile_id, *expires_at, namespace)?;
                Ok(vec![])
            }
            SceneMutation::DeleteTile { tile_id } => {
                // Use the checked delete which enforces namespace isolation and capabilities.
                self.delete_tile(*tile_id, namespace)?;
                Ok(vec![])
            }
            // ── Node mutations ────────────────────────────────────────────────
            SceneMutation::SetTileRoot {
                tile_id,
                node,
                descendants,
            } => {
                // Use checked variant to enforce namespace isolation and ModifyOwnTiles capability.
                // The whole inline subtree (root + descendants) materializes as one
                // mutation so it stays a coalescible StateStream update (hud-ga4md).
                self.set_tile_root_tree_checked(
                    *tile_id,
                    node.clone(),
                    descendants.clone(),
                    namespace,
                )?;
                Ok(vec![node.id])
            }
            SceneMutation::AddNode {
                tile_id,
                parent_id,
                node,
            } => {
                // Use checked variant to enforce namespace isolation and ModifyOwnTiles capability.
                self.add_node_to_tile_checked(*tile_id, *parent_id, node.clone(), namespace)?;
                Ok(vec![node.id])
            }
            SceneMutation::UpdateNodeContent {
                tile_id,
                node_id,
                data,
            } => {
                // Use checked variant to enforce namespace isolation and ModifyOwnTiles capability.
                self.update_node_content_checked(*tile_id, *node_id, data.clone(), namespace)?;
                Ok(vec![])
            }
            // ── Zone mutations ────────────────────────────────────────────────
            SceneMutation::PublishToZone {
                zone_name,
                content,
                publish_token: _publish_token, // token validated by the gRPC layer
                merge_key,
                expires_at_wall_us,
                content_classification,
                breakpoints,
            } => {
                if !breakpoints.is_empty() && matches!(content, ZoneContent::StreamText(_)) {
                    self.publish_to_zone_with_breakpoints(
                        zone_name,
                        content.clone(),
                        namespace,
                        merge_key.clone(),
                        *expires_at_wall_us,
                        content_classification.clone(),
                        breakpoints.clone(),
                    )?;
                } else {
                    self.publish_to_zone(
                        zone_name,
                        content.clone(),
                        namespace,
                        merge_key.clone(),
                        *expires_at_wall_us,
                        content_classification.clone(),
                    )?;
                }
                Ok(vec![])
            }
            SceneMutation::ClearZone {
                zone_name,
                publish_token: _publish_token, // token validated by the gRPC layer
            } => {
                // Per spec: ClearZone clears publications by THIS agent in the zone.
                self.clear_zone_for_publisher(zone_name, namespace)?;
                Ok(vec![])
            }
            SceneMutation::ClearWidget {
                widget_name,
                instance_id,
            } => {
                // ClearWidget clears publications by THIS agent on the widget.
                // Resolves the instance name: instance_id overrides widget_name when present.
                let resolved_name = instance_id
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(widget_name.as_str());
                self.clear_widget_for_publisher(resolved_name, namespace)?;
                Ok(vec![])
            }
            // ── Scroll mutations ─────────────────────────────────────────
            SceneMutation::RegisterTileScroll {
                tile_id,
                scrollable_x,
                scrollable_y,
                content_width,
                content_height,
            } => {
                let tile = self
                    .tiles
                    .get(tile_id)
                    .ok_or(ValidationError::TileNotFound { id: *tile_id })?;
                if tile.namespace != namespace {
                    return Err(ValidationError::NamespaceMismatch {
                        tile_id: *tile_id,
                        tile_namespace: tile.namespace.clone(),
                        agent_namespace: namespace.to_string(),
                    });
                }
                self.register_tile_scroll_config(
                    *tile_id,
                    TileScrollConfig {
                        scrollable_x: *scrollable_x,
                        scrollable_y: *scrollable_y,
                        content_width: *content_width,
                        content_height: *content_height,
                    },
                )?;
                Ok(vec![])
            }
            SceneMutation::SetScrollOffset {
                tile_id,
                offset_x,
                offset_y,
            } => {
                let tile = self
                    .tiles
                    .get(tile_id)
                    .ok_or(ValidationError::TileNotFound { id: *tile_id })?;
                if tile.namespace != namespace {
                    return Err(ValidationError::NamespaceMismatch {
                        tile_id: *tile_id,
                        tile_namespace: tile.namespace.clone(),
                        agent_namespace: namespace.to_string(),
                    });
                }
                self.set_tile_scroll_offset_local(*tile_id, *offset_x, *offset_y)?;
                Ok(vec![])
            }
            // ── Lifecycle affordance accent ──────────────────────────────
            SceneMutation::SetTileLifecycleAccent { tile_id, accent } => {
                // Checked variants enforce namespace isolation + live
                // lease/`ModifyOwnTiles` capability, matching the sibling content
                // mutations above (`SetTileRoot`, `UpdateTileInputMode`). `apply_batch`
                // Stage-1 already rejects a non-Active lease here, but routing through
                // the checked path keeps this arm defense-in-depth consistent and
                // closes the accent-overlay lease-suspension escape (hud-a745w).
                match accent {
                    Some(a) => self.set_tile_lifecycle_accent_checked(*tile_id, *a, namespace)?,
                    None => self.clear_tile_lifecycle_accent_checked(*tile_id, namespace)?,
                }
                Ok(vec![])
            }
            // ── Ambient unread-output count (jump-to-latest badge) ────────
            SceneMutation::SetTileUnreadCount { tile_id, count } => {
                // Checked path enforces namespace isolation + live lease +
                // `ModifyOwnTiles`, matching the sibling `SetTileLifecycleAccent`
                // and portal-surface arms (hud-a745w). A same-namespace session
                // whose `ModifyOwnTiles` was revoked (lease still Active) must not
                // keep mutating tile UI state — `apply_batch` Stage 1 only checks
                // lease liveness, not the capability.
                self.set_tile_unread_count_checked(*tile_id, *count, namespace)?;
                Ok(vec![])
            }
            // ── Composer interaction hit region (hud-iofav) ──────────────
            SceneMutation::SetTileComposerInteraction { tile_id, region } => {
                // Checked path enforces namespace isolation + live lease +
                // `ModifyOwnTiles`, matching the sibling `SetTileLifecycleAccent`
                // and `SetTileUnreadCount` arms (hud-a745w): a suspended/orphaned/
                // expired or capability-revoked lease must not attach the composer
                // node or bump `scene.version`. The derived hit region is runtime
                // overlay state, not a published element, so no `pending_touch_ids`
                // entry — the co-travelling `PublishToTile` content mutation's
                // repaint carries it.
                match region {
                    Some(r) => {
                        self.set_tile_composer_interaction_checked(*tile_id, r.clone(), namespace)?
                    }
                    None => self.clear_tile_composer_interaction_checked(*tile_id, namespace)?,
                }
                Ok(vec![])
            }
            // ── Portal surface (RFC 0013 §7.2 promotion) ─────────────────
            SceneMutation::SetPortalSurface { tile_id, surface } => {
                // Checked path enforces namespace isolation + live
                // lease/ModifyOwnTiles capability (hud-tc153 review P1).
                self.set_portal_surface(*tile_id, surface.clone(), namespace)?;
                Ok(vec![])
            }
            SceneMutation::UpdatePortalSurfaceState {
                tile_id,
                lifecycle,
                display_state,
            } => {
                self.update_portal_surface_state(*tile_id, *lifecycle, *display_state, namespace)?;
                Ok(vec![])
            }
        }
    }

    // remove_tile_and_nodes and remove_node_tree are defined in graph.rs as pub(crate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_batch(agent: &str, mutations: Vec<SceneMutation>) -> MutationBatch {
        MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: agent.to_string(),
            mutations,
            timing_hints: None,
            lease_id: None,
        }
    }

    fn make_batch_with_lease(
        agent: &str,
        lease_id: SceneId,
        mutations: Vec<SceneMutation>,
    ) -> MutationBatch {
        MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: agent.to_string(),
            mutations,
            timing_hints: None,
            lease_id: Some(lease_id),
        }
    }

    #[test]
    fn test_mutation_batch_apply() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        let batch = make_batch(
            "agent",
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".to_string(),
                lease_id,
                bounds: Rect::new(10.0, 10.0, 200.0, 150.0),
                z_order: 1,
            }],
        );

        let result = scene.apply_batch(&batch);
        assert!(result.applied);
        assert_eq!(result.created_ids.len(), 1);
        assert_eq!(scene.tile_count(), 1);
        assert!(result.sequence_number.is_some());
    }

    #[test]
    fn set_tile_unread_count_mutation_updates_overlay_and_enforces_namespace() {
        // hud-hwk2m: the wire `SetTileUnreadCount` mutation (what a bridged portal
        // sends to carry the jump-to-latest pill badge) must land on the runtime
        // overlay the compositor reads (`tile_unread_count`), with namespace
        // isolation matching the sibling content mutations.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let created = scene.apply_batch(&make_batch_with_lease(
            "agent",
            lease_id,
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".to_string(),
                lease_id,
                bounds: Rect::new(10.0, 10.0, 200.0, 150.0),
                z_order: 1,
            }],
        ));
        assert!(created.applied);
        let tile_id = created.created_ids[0];
        assert_eq!(
            scene.tile_unread_count(tile_id),
            0,
            "no badge before any count is set"
        );

        // Set the badge count. A count CHANGE must bump `scene.version` so a
        // count-only update re-arms the idle present-gate and the badge repaints
        // (mirrors SetTileLifecycleAccent).
        let version_before = scene.version;
        assert!(
            scene
                .apply_batch(&make_batch_with_lease(
                    "agent",
                    lease_id,
                    vec![SceneMutation::SetTileUnreadCount { tile_id, count: 5 }],
                ))
                .applied
        );
        assert_eq!(
            scene.tile_unread_count(tile_id),
            5,
            "the wire mutation must land on the compositor-read overlay"
        );
        assert!(
            scene.version > version_before,
            "a count change must bump scene.version so a count-only update repaints"
        );

        // A redundant re-write of the SAME count must NOT bump the version — a
        // steady-state portal stays idle.
        let version_steady = scene.version;
        assert!(
            scene
                .apply_batch(&make_batch_with_lease(
                    "agent",
                    lease_id,
                    vec![SceneMutation::SetTileUnreadCount { tile_id, count: 5 }],
                ))
                .applied
        );
        assert_eq!(
            scene.version, version_steady,
            "re-writing the same count must not bump scene.version"
        );

        // Count 0 clears the badge.
        assert!(
            scene
                .apply_batch(&make_batch_with_lease(
                    "agent",
                    lease_id,
                    vec![SceneMutation::SetTileUnreadCount { tile_id, count: 0 }],
                ))
                .applied
        );
        assert_eq!(
            scene.tile_unread_count(tile_id),
            0,
            "count 0 clears the badge"
        );

        // Re-set, then prove a cross-namespace write is rejected and leaves the
        // count untouched.
        assert!(
            scene
                .apply_batch(&make_batch_with_lease(
                    "agent",
                    lease_id,
                    vec![SceneMutation::SetTileUnreadCount { tile_id, count: 3 }],
                ))
                .applied
        );
        assert!(
            !scene
                .apply_batch(&make_batch(
                    "intruder",
                    vec![SceneMutation::SetTileUnreadCount { tile_id, count: 99 }],
                ))
                .applied,
            "a cross-namespace badge write must be rejected"
        );
        assert_eq!(
            scene.tile_unread_count(tile_id),
            3,
            "a rejected cross-namespace write must not change the count"
        );

        // Revoking `ModifyOwnTiles` (lease still Active, same namespace) must
        // reject the badge write with `CapabilityMissing` — parity with the
        // checked lifecycle-accent / portal-surface paths (hud-a745w). A
        // capability-revoked session must not keep mutating tile UI state.
        scene
            .revoke_capability(lease_id, &Capability::ModifyOwnTiles)
            .expect("revoke ModifyOwnTiles");
        assert!(
            !scene
                .apply_batch(&make_batch_with_lease(
                    "agent",
                    lease_id,
                    vec![SceneMutation::SetTileUnreadCount { tile_id, count: 7 }],
                ))
                .applied,
            "a ModifyOwnTiles-revoked session must not set the badge count"
        );
        assert_eq!(
            scene.tile_unread_count(tile_id),
            3,
            "a capability-rejected write must not change the count"
        );
    }

    #[test]
    fn accepted_batch_records_present_ack_and_drains_once() {
        // hud-91uu6: an accepted batch enqueues its batch_id for present-ack
        // correlation; the render loop drains it exactly once per present.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let batch = make_batch(
            "agent",
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".to_string(),
                lease_id,
                bounds: Rect::new(10.0, 10.0, 200.0, 150.0),
                z_order: 1,
            }],
        );
        let expected = batch.batch_id;

        assert!(scene.apply_batch(&batch).applied);

        // The applied batch_id is queued for the next present.
        let drained = scene.drain_present_ack_batch_ids();
        assert_eq!(drained, vec![expected], "drain yields the applied batch_id");

        // Drain is one-shot: a second drain (no new applies) is empty.
        assert!(
            scene.drain_present_ack_batch_ids().is_empty(),
            "second drain yields nothing"
        );
    }

    #[test]
    fn rejected_batch_records_no_present_ack() {
        // A rejected batch (missing lease) must not enqueue a present-ack: only
        // batches whose commit changed the scene are reflected by a frame.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        // Reference a lease that was never granted → Stage 1 rejection.
        let bogus_lease = SceneId::new();
        let batch = make_batch(
            "agent",
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".to_string(),
                lease_id: bogus_lease,
                bounds: Rect::new(10.0, 10.0, 200.0, 150.0),
                z_order: 1,
            }],
        );

        assert!(!scene.apply_batch(&batch).applied, "batch must be rejected");
        assert!(
            scene.drain_present_ack_batch_ids().is_empty(),
            "rejected batch enqueues no present-ack"
        );
    }

    #[test]
    fn present_ack_preserves_application_order_across_batches() {
        // Multiple batches applied before a present drain in application order.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let mut expected = Vec::new();
        for i in 0..3 {
            let batch = make_batch(
                "agent",
                vec![SceneMutation::CreateTile {
                    tab_id,
                    namespace: "agent".to_string(),
                    lease_id,
                    bounds: Rect::new(10.0 * (i + 1) as f32, 10.0, 100.0, 80.0),
                    z_order: i + 1,
                }],
            );
            expected.push(batch.batch_id);
            assert!(scene.apply_batch(&batch).applied);
        }
        assert_eq!(scene.drain_present_ack_batch_ids(), expected);
    }

    #[test]
    fn set_tile_lifecycle_accent_sets_clears_and_cleans_up() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let create = make_batch(
            "agent",
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".to_string(),
                lease_id,
                bounds: Rect::new(10.0, 10.0, 200.0, 150.0),
                z_order: 1,
            }],
        );
        let tile_id = scene.apply_batch(&create).created_ids[0];

        // Set: the accent lands in overlay state, keyed by tile id, and bumps
        // scene.version so the #943 idle present-gate re-arms for an accent-only
        // transition (regression guard for hud-m48i0).
        let accent = LifecycleAccent {
            color: Rgba::new(0.1, 0.2, 0.3, 1.0),
            width_px: 4.0,
        };
        let set = make_batch(
            "agent",
            vec![SceneMutation::SetTileLifecycleAccent {
                tile_id,
                accent: Some(accent),
            }],
        );
        let v_before_set = scene.version;
        assert!(scene.apply_batch(&set).applied);
        assert_eq!(scene.tile_lifecycle_accent(tile_id), Some(accent));
        assert!(
            scene.version > v_before_set,
            "setting a new accent must bump scene.version so the present-gate repaints"
        );

        // Redundant set (same accent) is a no-op for the present-gate: version
        // must NOT change, so a steady-state portal stays idle.
        let v_before_redundant = scene.version;
        let set_same = make_batch(
            "agent",
            vec![SceneMutation::SetTileLifecycleAccent {
                tile_id,
                accent: Some(accent),
            }],
        );
        assert!(scene.apply_batch(&set_same).applied);
        assert_eq!(
            scene.version, v_before_redundant,
            "re-publishing the same accent must not bump version (keeps idle idle)"
        );

        // Clear: None removes it and bumps version (the redaction-CLEAR path).
        let clear = make_batch(
            "agent",
            vec![SceneMutation::SetTileLifecycleAccent {
                tile_id,
                accent: None,
            }],
        );
        let v_before_clear = scene.version;
        assert!(scene.apply_batch(&clear).applied);
        assert_eq!(scene.tile_lifecycle_accent(tile_id), None);
        assert!(
            scene.version > v_before_clear,
            "clearing a present accent must bump scene.version (stale accent must repaint away)"
        );

        // Clearing again (nothing stored) must NOT bump version.
        let v_before_noop_clear = scene.version;
        let clear_noop = make_batch(
            "agent",
            vec![SceneMutation::SetTileLifecycleAccent {
                tile_id,
                accent: None,
            }],
        );
        assert!(scene.apply_batch(&clear_noop).applied);
        assert_eq!(
            scene.version, v_before_noop_clear,
            "clearing an absent accent must not bump version"
        );

        // Cleanup: deleting the tile drops any accent overlay state.
        let set_again = make_batch(
            "agent",
            vec![SceneMutation::SetTileLifecycleAccent {
                tile_id,
                accent: Some(accent),
            }],
        );
        assert!(scene.apply_batch(&set_again).applied);
        assert_eq!(scene.tile_lifecycle_accent(tile_id), Some(accent));
        let delete = make_batch("agent", vec![SceneMutation::DeleteTile { tile_id }]);
        assert!(scene.apply_batch(&delete).applied);
        assert_eq!(
            scene.tile_lifecycle_accent(tile_id),
            None,
            "accent overlay state must be cleaned up on tile deletion"
        );
    }

    /// Build a leased scene with a portal tile at the display origin whose root is
    /// a TextMarkdown "transcript" node — the shape an interaction-enabled portal
    /// republishes each streaming render (hud-iofav).
    #[cfg(test)]
    fn transcript_scene_with_tile(text: &str) -> (SceneGraph, SceneId) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let create = make_batch(
            "agent",
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".to_string(),
                lease_id,
                bounds: Rect::new(0.0, 0.0, 300.0, 200.0),
                z_order: 1,
            }],
        );
        let tile_id = scene.apply_batch(&create).created_ids[0];
        let set_root = make_batch(
            "agent",
            vec![SceneMutation::SetTileRoot {
                tile_id,
                node: transcript_root(text),
                descendants: vec![],
            }],
        );
        assert!(scene.apply_batch(&set_root).applied);
        (scene, tile_id)
    }

    #[cfg(test)]
    fn transcript_root(text: &str) -> Node {
        Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: text.to_string(),
                bounds: Rect::new(0.0, 0.0, 300.0, 200.0),
                font_size_px: 14.0,
                font_family: FontFamily::default(),
                color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                background: None,
                alignment: TextAlign::default(),
                overflow: TextOverflow::default(),
                color_runs: Box::default(),
            }),
        }
    }

    #[cfg(test)]
    fn composer_region() -> HitRegionNode {
        HitRegionNode {
            bounds: Rect::new(0.0, 0.0, 300.0, 200.0),
            interaction_id: "portal-42-composer".to_string(),
            accepts_focus: true,
            accepts_pointer: true,
            accepts_composer_input: true,
            ..Default::default()
        }
    }

    /// Return the tile root's HitRegion child that accepts composer input, if any.
    #[cfg(test)]
    fn composer_child(scene: &SceneGraph, tile_id: SceneId) -> Option<(SceneId, HitRegionNode)> {
        let root_id = scene.tiles.get(&tile_id)?.root_node?;
        let root = scene.nodes.get(&root_id)?;
        for child_id in &root.children {
            if let Some(child) = scene.nodes.get(child_id) {
                if let NodeData::HitRegion(hr) = &child.data {
                    if hr.accepts_composer_input {
                        return Some((*child_id, hr.clone()));
                    }
                }
            }
        }
        None
    }

    /// The interaction-path regression guard for hud-iofav (the missing counterpart
    /// to the hud-mzk74 non-interactive tests): a `SetTileComposerInteraction`
    /// attaches a composer hit-region node under the tile root, and that node —
    /// with `accepts_composer_input` and click-to-focus `accepts_pointer` — SURVIVES
    /// consecutive transcript republishes (each `SetTileRoot` replaces the whole
    /// node tree). The scene re-derives it from overlay state, so the streaming
    /// render never has to re-send a per-republish `AddNode` (which would flip the
    /// batch Transactional and defeat StateStream coalescing on the hottest path).
    #[test]
    fn composer_interaction_hit_region_survives_consecutive_transcript_republishes() {
        let (mut scene, tile_id) = transcript_scene_with_tile("render 1");

        // Enable interaction: the composer hit region attaches under the root.
        let enable = make_batch(
            "agent",
            vec![SceneMutation::SetTileComposerInteraction {
                tile_id,
                region: Some(composer_region()),
            }],
        );
        assert!(scene.apply_batch(&enable).applied);

        let (composer_id_1, hr1) =
            composer_child(&scene, tile_id).expect("composer hit region must attach on enable");
        assert!(
            hr1.accepts_composer_input,
            "attached composer must accept composer input"
        );
        assert!(
            hr1.accepts_pointer,
            "attached composer must accept pointer (click-to-focus, hud-v4k1h)"
        );
        assert_eq!(hr1.interaction_id, "portal-42-composer");
        // Click-to-focus resolves to the composer node via hit_test (the tile sits
        // at the display origin, so display coords == tile-local coords here).
        assert_eq!(
            scene.hit_test(150.0, 100.0),
            HitResult::NodeHit {
                tile_id,
                node_id: composer_id_1,
                interaction_id: "portal-42-composer".to_string(),
            },
            "a click inside the composer must resolve to the composer NodeHit"
        );

        // Republish the transcript root twice (consecutive streaming renders). Each
        // republish replaces the whole node tree; the composer must be re-derived.
        for (i, text) in ["render 2", "render 3"].iter().enumerate() {
            let republish = make_batch(
                "agent",
                vec![SceneMutation::SetTileRoot {
                    tile_id,
                    node: transcript_root(text),
                    descendants: vec![],
                }],
            );
            assert!(scene.apply_batch(&republish).applied);

            // The transcript content updated…
            let root_id = scene.tiles.get(&tile_id).unwrap().root_node.unwrap();
            match &scene.nodes.get(&root_id).unwrap().data {
                NodeData::TextMarkdown(tm) => assert_eq!(&tm.content, text),
                other => panic!("root must be TextMarkdown after republish, got {other:?}"),
            }

            // …and the composer hit region SURVIVED, still accepting composer input
            // and still click-to-focus.
            let (composer_id, hr) = composer_child(&scene, tile_id)
                .unwrap_or_else(|| panic!("composer hit region must survive republish {}", i + 2));
            assert!(
                hr.accepts_composer_input,
                "composer must still accept composer input after republish {}",
                i + 2
            );
            assert_eq!(
                scene.hit_test(150.0, 100.0),
                HitResult::NodeHit {
                    tile_id,
                    node_id: composer_id,
                    interaction_id: "portal-42-composer".to_string(),
                },
                "composer must remain click-to-focus after republish {}",
                i + 2
            );
        }

        // Disable interaction: the derived composer node is detached.
        let disable = make_batch(
            "agent",
            vec![SceneMutation::SetTileComposerInteraction {
                tile_id,
                region: None,
            }],
        );
        assert!(scene.apply_batch(&disable).applied);
        assert!(
            composer_child(&scene, tile_id).is_none(),
            "disabling interaction must detach the composer hit region"
        );
        assert!(
            !scene.hit_test(150.0, 100.0).is_node_hit(),
            "after disable a click must no longer resolve to a composer NodeHit"
        );
    }

    /// A `SetTileComposerInteraction` authored by an agent that does NOT own the
    /// tile must be rejected (lease/namespace gate), leaving no composer node
    /// attached — mirroring the accent's cross-namespace guard (hud-a745w).
    #[test]
    fn set_tile_composer_interaction_rejects_cross_namespace() {
        let (mut scene, tile_id) = transcript_scene_with_tile("render 1");
        // A different namespace with its own lease must not attach a composer.
        let _intruder = scene.grant_lease(
            "intruder",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let hostile = make_batch(
            "intruder",
            vec![SceneMutation::SetTileComposerInteraction {
                tile_id,
                region: Some(composer_region()),
            }],
        );
        let result = scene.apply_batch(&hostile);
        assert!(
            !result.applied,
            "cross-namespace composer interaction must be rejected"
        );
        assert!(
            composer_child(&scene, tile_id).is_none(),
            "a rejected composer interaction must not attach a hit region"
        );
    }

    /// A `SetTileLifecycleAccent` authored by an agent that does NOT own the tile
    /// must be rejected with `NamespaceMismatch` and must not mutate the accent
    /// overlay state — the accent is a per-tile visual owned by the tile's
    /// namespace, exactly like every other tile mutation (hud-m48i0).
    #[test]
    fn set_tile_lifecycle_accent_rejects_cross_namespace() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let owner_lease = scene.grant_lease(
            "owner",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let create = make_batch(
            "owner",
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "owner".to_string(),
                lease_id: owner_lease,
                bounds: Rect::new(10.0, 10.0, 200.0, 150.0),
                z_order: 1,
            }],
        );
        let tile_id = scene.apply_batch(&create).created_ids[0];

        // Intruder (a different namespace) tries to set the accent on owner's tile.
        let intruder = make_batch(
            "intruder",
            vec![SceneMutation::SetTileLifecycleAccent {
                tile_id,
                accent: Some(LifecycleAccent {
                    color: Rgba::new(0.9, 0.1, 0.1, 1.0),
                    width_px: 4.0,
                }),
            }],
        );
        let result = scene.apply_batch(&intruder);
        assert!(
            !result.applied,
            "cross-namespace accent write must be rejected"
        );
        let rej = result
            .rejection
            .expect("a structured rejection must be present");
        assert_eq!(rej.errors[0].code, ValidationErrorCode::NamespaceMismatch);
        assert_eq!(
            scene.tile_lifecycle_accent(tile_id),
            None,
            "rejected cross-namespace write must not mutate accent overlay state"
        );
    }

    // ── Portal surface (RFC 0013 §7.2 promotion; hud-tc153) ──────────────────

    /// Set up a tab + lease + tile whose root is a known SolidColor node, and
    /// return `(scene, tile_id, root_node_id)`. Mirrors the raw-tile pilot: the
    /// tile's content node is what a portal part points at.
    fn portal_scene_with_tile() -> (SceneGraph, SceneId, SceneId) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let create = make_batch(
            "agent",
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".to_string(),
                lease_id,
                bounds: Rect::new(10.0, 10.0, 400.0, 300.0),
                z_order: 1,
            }],
        );
        let tile_id = scene.apply_batch(&create).created_ids[0];
        let root = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::SolidColor(SolidColorNode {
                color: Rgba::new(0.0, 0.0, 0.0, 1.0),
                bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                radius: None,
            }),
        };
        let root_id = root.id;
        let set_root = make_batch(
            "agent",
            vec![SceneMutation::SetTileRoot {
                tile_id,
                node: root,
                descendants: vec![],
            }],
        );
        assert!(scene.apply_batch(&set_root).applied);
        (scene, tile_id, root_id)
    }

    /// The Phase-0 raw-tile assembly is expressible via the promoted surface:
    /// a `SetPortalSurface` declaring all eight named parts lands in overlay
    /// state, bumps `scene.version`, and reads back verbatim.
    #[test]
    fn set_portal_surface_expresses_all_eight_parts() {
        let (mut scene, tile_id, root_id) = portal_scene_with_tile();

        // All eight parts; the transcript part points at the tile's content node
        // (raw-tile expression), the rest carry geometry only.
        let parts: Vec<PortalPart> = PortalPartKind::ALL
            .iter()
            .enumerate()
            .map(|(i, &kind)| PortalPart {
                kind,
                bounds: Rect::new(0.0, i as f32 * 10.0, 400.0, 10.0),
                node: if kind == PortalPartKind::Transcript {
                    Some(root_id)
                } else {
                    None
                },
            })
            .collect();
        let surface = PortalSurface {
            identity: PortalIdentity {
                session_id: "sess-1".to_string(),
                display_name: "Claude".to_string(),
                peer_class: PortalPeerClass::ResidentLlm,
            },
            lifecycle: PortalLifecycleState::Active,
            display_state: PortalDisplayState::Expanded,
            parts,
        };

        let v_before = scene.version;
        let set = make_batch(
            "agent",
            vec![SceneMutation::SetPortalSurface {
                tile_id,
                surface: surface.clone(),
            }],
        );
        assert!(
            scene.apply_batch(&set).applied,
            "SetPortalSurface must apply"
        );
        assert_eq!(scene.portal_surface(tile_id), Some(&surface));
        assert!(
            scene.version > v_before,
            "declaring a portal surface must bump scene.version"
        );

        // Redundant re-declare of the same surface is a version no-op (idle stays idle).
        let v_before_redundant = scene.version;
        let set_same = make_batch(
            "agent",
            vec![SceneMutation::SetPortalSurface { tile_id, surface }],
        );
        assert!(scene.apply_batch(&set_same).applied);
        assert_eq!(scene.version, v_before_redundant);

        // Cleanup on tile deletion.
        let delete = make_batch("agent", vec![SceneMutation::DeleteTile { tile_id }]);
        assert!(scene.apply_batch(&delete).applied);
        assert_eq!(
            scene.portal_surface(tile_id),
            None,
            "portal surface overlay state must be cleaned up on tile deletion"
        );
    }

    /// `UpdatePortalSurfaceState` coalesces: a `None` field is left unchanged and
    /// a redundant patch does not bump `scene.version`.
    #[test]
    fn update_portal_surface_state_patches_and_coalesces() {
        let (mut scene, tile_id, _root_id) = portal_scene_with_tile();
        let set = make_batch(
            "agent",
            vec![SceneMutation::SetPortalSurface {
                tile_id,
                surface: PortalSurface {
                    lifecycle: PortalLifecycleState::Active,
                    display_state: PortalDisplayState::Expanded,
                    ..Default::default()
                },
            }],
        );
        assert!(scene.apply_batch(&set).applied);

        // Patch only display_state; lifecycle (None) is left unchanged.
        let patch = make_batch(
            "agent",
            vec![SceneMutation::UpdatePortalSurfaceState {
                tile_id,
                lifecycle: None,
                display_state: Some(PortalDisplayState::Collapsed),
            }],
        );
        let v_before = scene.version;
        assert!(scene.apply_batch(&patch).applied);
        let s = scene.portal_surface(tile_id).unwrap();
        assert_eq!(
            s.lifecycle,
            PortalLifecycleState::Active,
            "lifecycle unchanged"
        );
        assert_eq!(
            s.display_state,
            PortalDisplayState::Collapsed,
            "display patched"
        );
        assert!(
            scene.version > v_before,
            "a real state change bumps version"
        );

        // Redundant patch (same values) is a version no-op.
        let v_before_redundant = scene.version;
        let patch_same = make_batch(
            "agent",
            vec![SceneMutation::UpdatePortalSurfaceState {
                tile_id,
                lifecycle: None,
                display_state: Some(PortalDisplayState::Collapsed),
            }],
        );
        assert!(scene.apply_batch(&patch_same).applied);
        assert_eq!(scene.version, v_before_redundant);
    }

    /// Patching a tile that has no declared portal surface is rejected with
    /// `PortalSurfaceNotFound` (you must `SetPortalSurface` first).
    #[test]
    fn update_portal_surface_state_requires_prior_surface() {
        let (mut scene, tile_id, _root_id) = portal_scene_with_tile();
        let patch = make_batch(
            "agent",
            vec![SceneMutation::UpdatePortalSurfaceState {
                tile_id,
                lifecycle: Some(PortalLifecycleState::Blocked),
                display_state: None,
            }],
        );
        let result = scene.apply_batch(&patch);
        assert!(!result.applied);
        assert_eq!(
            result.rejection.unwrap().errors[0].code,
            ValidationErrorCode::PortalSurfaceNotFound
        );
    }

    /// A portal part referencing a node that is not in the host tile's tree is
    /// rejected with `InvalidPortalSurface` (a surface cannot borrow foreign nodes).
    #[test]
    fn set_portal_surface_rejects_foreign_part_node() {
        let (mut scene, tile_id, _root_id) = portal_scene_with_tile();
        let set = make_batch(
            "agent",
            vec![SceneMutation::SetPortalSurface {
                tile_id,
                surface: PortalSurface {
                    parts: vec![PortalPart {
                        kind: PortalPartKind::Transcript,
                        bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                        node: Some(SceneId::new()), // not in the tile
                    }],
                    ..Default::default()
                },
            }],
        );
        let result = scene.apply_batch(&set);
        assert!(!result.applied);
        assert_eq!(
            result.rejection.unwrap().errors[0].code,
            ValidationErrorCode::InvalidPortalSurface
        );
    }

    /// Cross-namespace `SetPortalSurface` is rejected and mutates no overlay state.
    #[test]
    fn set_portal_surface_rejects_cross_namespace() {
        let (mut scene, tile_id, _root_id) = portal_scene_with_tile();
        let intruder = make_batch(
            "intruder",
            vec![SceneMutation::SetPortalSurface {
                tile_id,
                surface: PortalSurface::default(),
            }],
        );
        let result = scene.apply_batch(&intruder);
        assert!(!result.applied);
        assert_eq!(
            result.rejection.unwrap().errors[0].code,
            ValidationErrorCode::NamespaceMismatch
        );
        assert_eq!(scene.portal_surface(tile_id), None);
    }

    /// hud-tc153 review P1: a portal surface is a content-layer, lease-governed
    /// object, so once `ModifyOwnTiles` is revoked mid-lease both
    /// `SetPortalSurface` and `UpdatePortalSurfaceState` must be rejected with
    /// `CapabilityMissing` — an active lease alone is not sufficient authority.
    #[test]
    fn portal_surface_mutations_require_live_modify_capability() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let create = make_batch(
            "agent",
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".to_string(),
                lease_id,
                bounds: Rect::new(10.0, 10.0, 400.0, 300.0),
                z_order: 1,
            }],
        );
        let tile_id = scene.apply_batch(&create).created_ids[0];

        // Declare a surface while the capability is still present — succeeds.
        let declare = make_batch(
            "agent",
            vec![SceneMutation::SetPortalSurface {
                tile_id,
                surface: PortalSurface {
                    lifecycle: PortalLifecycleState::Active,
                    display_state: PortalDisplayState::Expanded,
                    ..Default::default()
                },
            }],
        );
        assert!(scene.apply_batch(&declare).applied);

        // Revoke ModifyOwnTiles; the lease stays active.
        scene
            .revoke_capability(lease_id, &Capability::ModifyOwnTiles)
            .expect("revoke must succeed");

        // SetPortalSurface is now blocked by the missing capability.
        let set = make_batch(
            "agent",
            vec![SceneMutation::SetPortalSurface {
                tile_id,
                surface: PortalSurface {
                    lifecycle: PortalLifecycleState::Blocked,
                    ..Default::default()
                },
            }],
        );
        let set_result = scene.apply_batch(&set);
        assert!(!set_result.applied, "SetPortalSurface must be rejected");
        assert_eq!(
            set_result.rejection.unwrap().errors[0].code,
            ValidationErrorCode::CapabilityMissing
        );

        // UpdatePortalSurfaceState is likewise blocked.
        let patch = make_batch(
            "agent",
            vec![SceneMutation::UpdatePortalSurfaceState {
                tile_id,
                lifecycle: Some(PortalLifecycleState::Blocked),
                display_state: None,
            }],
        );
        let patch_result = scene.apply_batch(&patch);
        assert!(
            !patch_result.applied,
            "UpdatePortalSurfaceState must be rejected"
        );
        assert_eq!(
            patch_result.rejection.unwrap().errors[0].code,
            ValidationErrorCode::CapabilityMissing
        );

        // The originally declared state is untouched by the rejected writes.
        let surviving = scene.portal_surface(tile_id).unwrap();
        assert_eq!(surviving.lifecycle, PortalLifecycleState::Active);
        assert_eq!(surviving.display_state, PortalDisplayState::Expanded);
    }

    /// hud-tc153 review P2: the descriptor survives a content republish, but a
    /// part node reference that pointed at the now-removed root must not dangle —
    /// `SetTileRoot` prunes it back to `None` so consumers never resolve a stale
    /// `SceneId`.
    #[test]
    fn portal_part_node_refs_pruned_on_root_republish() {
        let (mut scene, tile_id, root_id) = portal_scene_with_tile();

        // Declare a surface whose transcript part points at the current root node.
        let declare = make_batch(
            "agent",
            vec![SceneMutation::SetPortalSurface {
                tile_id,
                surface: PortalSurface {
                    parts: vec![PortalPart {
                        kind: PortalPartKind::Transcript,
                        bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                        node: Some(root_id),
                    }],
                    ..Default::default()
                },
            }],
        );
        assert!(scene.apply_batch(&declare).applied);
        assert_eq!(
            scene.portal_surface(tile_id).unwrap().parts[0].node,
            Some(root_id),
            "part must reference the declared node before republish"
        );

        // Republish the tile root with a fresh node tree (the old root is removed).
        let new_root = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::SolidColor(SolidColorNode {
                color: Rgba::new(0.1, 0.1, 0.1, 1.0),
                bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                radius: None,
            }),
        };
        let new_root_id = new_root.id;
        let republish = make_batch(
            "agent",
            vec![SceneMutation::SetTileRoot {
                tile_id,
                node: new_root,
                descendants: vec![],
            }],
        );
        assert!(scene.apply_batch(&republish).applied);

        // The descriptor survives, but the dangling ref to the removed old root is
        // pruned to None (not left pointing at a node no longer in the graph, and
        // not silently repointed at the new root).
        let part = &scene.portal_surface(tile_id).unwrap().parts[0];
        assert_eq!(
            part.node, None,
            "stale part node ref must be pruned to None on root republish"
        );
        assert_ne!(
            part.node,
            Some(new_root_id),
            "prune must not silently adopt the new root as the part's node"
        );
    }

    /// hud-tc153 review (Gemini): a portal part with negative width/height is
    /// rejected as an invalid portal surface, mirroring the non-negative extent
    /// expected of tile/node bounds.
    #[test]
    fn set_portal_surface_rejects_negative_part_bounds() {
        let (mut scene, tile_id, _root_id) = portal_scene_with_tile();
        let set = make_batch(
            "agent",
            vec![SceneMutation::SetPortalSurface {
                tile_id,
                surface: PortalSurface {
                    parts: vec![PortalPart {
                        kind: PortalPartKind::Frame,
                        bounds: Rect::new(0.0, 0.0, -10.0, 20.0),
                        node: None,
                    }],
                    ..Default::default()
                },
            }],
        );
        let result = scene.apply_batch(&set);
        assert!(
            !result.applied,
            "negative-extent part bounds must be rejected"
        );
        assert_eq!(
            result.rejection.unwrap().errors[0].code,
            ValidationErrorCode::InvalidPortalSurface
        );
        assert_eq!(
            scene.portal_surface(tile_id),
            None,
            "rejected write must not store overlay state"
        );
    }

    #[test]
    fn test_mutation_batch_rollback_on_failure() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        let batch = make_batch(
            "agent",
            vec![
                SceneMutation::CreateTile {
                    tab_id,
                    namespace: "agent".to_string(),
                    lease_id,
                    bounds: Rect::new(10.0, 10.0, 200.0, 150.0),
                    z_order: 1,
                },
                // This should fail — invalid bounds
                SceneMutation::CreateTile {
                    tab_id,
                    namespace: "agent".to_string(),
                    lease_id,
                    bounds: Rect::new(10.0, 10.0, 0.0, 0.0), // invalid
                    z_order: 2,
                },
            ],
        );

        let result = scene.apply_batch(&batch);
        assert!(!result.applied);
        // Entire batch rolled back — no tiles created
        assert_eq!(scene.tile_count(), 0);
        // Structured rejection must be present
        assert!(result.rejection.is_some());
        let rej = result.rejection.unwrap();
        assert_eq!(rej.errors[0].mutation_index, Some(1));
        // Zero-size bounds is a BoundsInvalid violation (width/height must be > 0.0)
        assert_eq!(rej.errors[0].code, ValidationErrorCode::BoundsInvalid);
    }

    #[test]
    fn test_batch_size_exceeded() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Build a batch with 1001 mutations
        let mutations: Vec<SceneMutation> = (0..=1000)
            .map(|z| SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".to_string(),
                lease_id,
                bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
                z_order: z as u32,
            })
            .collect();

        let batch = make_batch("agent", mutations);
        let result = scene.apply_batch(&batch);
        assert!(!result.applied);
        let rej = result.rejection.unwrap();
        assert_eq!(
            rej.primary_code(),
            Some(ValidationErrorCode::BatchSizeExceeded)
        );
        // No tiles created
        assert_eq!(scene.tile_count(), 0);
    }

    #[test]
    fn test_lease_check_before_budget_check() {
        // Stage 1 (lease check) must fire before Stage 2 (budget check).
        // We set an expired lease; the rejection must be LeaseExpired / LeaseInvalidState,
        // not BudgetExceeded.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();

        // Grant a lease with a 1ms TTL, then immediately expire it
        let lease_id = scene.grant_lease(
            "agent",
            1,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        // Advance the clock past TTL by expiring leases (simulated by direct state manipulation)
        scene.leases.get_mut(&lease_id).unwrap().state = crate::types::LeaseState::Expired;

        let batch = make_batch(
            "agent",
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".to_string(),
                lease_id,
                bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                z_order: 1,
            }],
        );

        let result = scene.apply_batch(&batch);
        assert!(!result.applied);
        let rej = result.rejection.unwrap();
        let code = rej.primary_code().unwrap();
        // Must be a lease-stage error, not a budget error
        assert!(
            matches!(
                code,
                ValidationErrorCode::LeaseInvalidState
                    | ValidationErrorCode::LeaseExpired
                    | ValidationErrorCode::LeaseNotFound
            ),
            "expected lease-stage error, got {code:?}"
        );
    }

    #[test]
    fn test_sequence_numbers_monotonically_increasing() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        let mut prev_seq = 0u64;
        for z in 1..=5u32 {
            let batch = make_batch(
                "agent",
                vec![SceneMutation::CreateTile {
                    tab_id,
                    namespace: "agent".to_string(),
                    lease_id,
                    bounds: Rect::new(z as f32 * 10.0, 0.0, 50.0, 50.0),
                    z_order: z,
                }],
            );
            let result = scene.apply_batch(&batch);
            assert!(result.applied, "batch {z} failed");
            let seq = result.sequence_number.unwrap();
            assert!(
                seq > prev_seq,
                "sequence {seq} not strictly greater than {prev_seq}"
            );
            prev_seq = seq;
        }
    }

    #[test]
    fn test_z_order_conflict_detected() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Create first tile at z_order=1 with bounds [0,0,200,200]
        let b1 = make_batch(
            "agent",
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".to_string(),
                lease_id,
                bounds: Rect::new(0.0, 0.0, 200.0, 200.0),
                z_order: 1,
            }],
        );
        let r1 = scene.apply_batch(&b1);
        assert!(r1.applied);

        // Try to create a second tile at same z_order=1 with overlapping bounds
        let b2 = make_batch(
            "agent",
            vec![SceneMutation::CreateTile {
                tab_id,
                namespace: "agent".to_string(),
                lease_id,
                bounds: Rect::new(100.0, 100.0, 200.0, 200.0), // overlaps first tile
                z_order: 1,                                    // same z_order
            }],
        );
        let r2 = scene.apply_batch(&b2);
        assert!(!r2.applied, "should reject z-order conflict");
        let rej = r2.rejection.unwrap();
        assert_eq!(
            rej.primary_code(),
            Some(ValidationErrorCode::ZOrderConflict)
        );
    }

    // ── UpdateNodeContent tests ──────────────────────────────────────────

    /// Helper: create a scene, tab, lease (with ModifyOwnTiles), tile, and text root node.
    fn make_scene_with_text_node() -> (SceneGraph, SceneId, SceneId, SceneId, SceneId) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 400.0, 300.0),
                1,
            )
            .unwrap();
        let node = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: "hello".to_string(),
                bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                font_size_px: 14.0,
                font_family: FontFamily::SystemSansSerif,
                color: Rgba {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                background: None,
                alignment: TextAlign::Start,
                overflow: TextOverflow::Clip,
                color_runs: Box::default(),
            }),
        };
        let node_id = node.id;
        scene.set_tile_root(tile_id, node).unwrap();
        (scene, tab_id, lease_id, tile_id, node_id)
    }

    #[test]
    fn test_update_node_content_text_happy_path() {
        let (mut scene, _tab, lease_id, tile_id, node_id) = make_scene_with_text_node();

        let batch = make_batch_with_lease(
            "agent",
            lease_id,
            vec![SceneMutation::UpdateNodeContent {
                tile_id,
                node_id,
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: "updated content".to_string(),
                    bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                    font_size_px: 16.0,
                    font_family: FontFamily::SystemSansSerif,
                    color: Rgba {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    },
                    background: None,
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Clip,
                    color_runs: Box::default(),
                }),
            }],
        );
        let result = scene.apply_batch(&batch);
        assert!(result.applied, "UpdateNodeContent should succeed");
        assert!(result.created_ids.is_empty(), "no new nodes created");

        // Verify the content was updated in-place.
        match &scene.nodes[&node_id].data {
            NodeData::TextMarkdown(tm) => {
                assert_eq!(tm.content, "updated content");
                assert_eq!(tm.font_size_px, 16.0);
            }
            _ => panic!("unexpected node data variant"),
        }
    }

    #[test]
    fn test_update_node_content_wrong_type_rejected() {
        let (mut scene, _tab, lease_id, tile_id, node_id) = make_scene_with_text_node();

        // Try to swap a TextMarkdown node for a SolidColor node — must be rejected.
        let batch = make_batch_with_lease(
            "agent",
            lease_id,
            vec![SceneMutation::UpdateNodeContent {
                tile_id,
                node_id,
                data: NodeData::SolidColor(SolidColorNode {
                    color: Rgba {
                        r: 1.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    },
                    bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                    radius: None,
                }),
            }],
        );
        let result = scene.apply_batch(&batch);
        assert!(!result.applied, "type-change should be rejected");
        let rej = result.rejection.unwrap();
        // Must be an InvalidField error targeting the data discriminant mismatch.
        assert_eq!(rej.errors[0].mutation_type, "UpdateNodeContent");
        assert_eq!(
            rej.primary_code(),
            Some(ValidationErrorCode::InvalidField),
            "type-change must produce an InvalidField error code"
        );
    }

    #[test]
    fn test_update_node_content_nonexistent_node_rejected() {
        let (mut scene, _tab, lease_id, tile_id, _node_id) = make_scene_with_text_node();
        let ghost_node_id = SceneId::new();

        let batch = make_batch_with_lease(
            "agent",
            lease_id,
            vec![SceneMutation::UpdateNodeContent {
                tile_id,
                node_id: ghost_node_id,
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: "nope".to_string(),
                    bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                    font_size_px: 14.0,
                    font_family: FontFamily::SystemSansSerif,
                    color: Rgba {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 1.0,
                    },
                    background: None,
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Clip,
                    color_runs: Box::default(),
                }),
            }],
        );
        let result = scene.apply_batch(&batch);
        assert!(!result.applied, "unknown node should be rejected");
        let rej = result.rejection.unwrap();
        assert_eq!(rej.primary_code(), Some(ValidationErrorCode::NodeNotFound));
    }

    #[test]
    fn test_update_node_content_node_in_wrong_tile_rejected() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        // Tile A with a text node.
        let tile_a = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 200.0, 100.0),
                1,
            )
            .unwrap();
        let node_a = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: "A".to_string(),
                bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                font_size_px: 14.0,
                font_family: FontFamily::SystemSansSerif,
                color: Rgba {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                background: None,
                alignment: TextAlign::Start,
                overflow: TextOverflow::Clip,
                color_runs: Box::default(),
            }),
        };
        let node_a_id = node_a.id;
        scene.set_tile_root(tile_a, node_a).unwrap();

        // Tile B (separate tile).
        let tile_b = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(300.0, 0.0, 200.0, 100.0),
                2,
            )
            .unwrap();

        // Try to update node_a using tile_b as the tile_id — must be rejected.
        let batch = make_batch_with_lease(
            "agent",
            lease_id,
            vec![SceneMutation::UpdateNodeContent {
                tile_id: tile_b,
                node_id: node_a_id,
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: "hijacked".to_string(),
                    bounds: Rect::new(0.0, 0.0, 200.0, 100.0),
                    font_size_px: 14.0,
                    font_family: FontFamily::SystemSansSerif,
                    color: Rgba {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 1.0,
                    },
                    background: None,
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Clip,
                    color_runs: Box::default(),
                }),
            }],
        );
        let result = scene.apply_batch(&batch);
        assert!(!result.applied, "cross-tile node access must be rejected");
        // Node A content must be unchanged.
        match &scene.nodes[&node_a_id].data {
            NodeData::TextMarkdown(tm) => assert_eq!(tm.content, "A"),
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn test_update_node_content_content_size_enforced() {
        use crate::graph::MAX_MARKDOWN_BYTES;
        let (mut scene, _tab, lease_id, tile_id, node_id) = make_scene_with_text_node();

        // Create a string that exceeds the markdown byte limit.
        let oversized = "x".repeat(MAX_MARKDOWN_BYTES + 1);

        let batch = make_batch_with_lease(
            "agent",
            lease_id,
            vec![SceneMutation::UpdateNodeContent {
                tile_id,
                node_id,
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: oversized,
                    bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                    font_size_px: 14.0,
                    font_family: FontFamily::SystemSansSerif,
                    color: Rgba {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 1.0,
                    },
                    background: None,
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Clip,
                    color_runs: Box::default(),
                }),
            }],
        );
        let result = scene.apply_batch(&batch);
        assert!(!result.applied, "oversized content must be rejected");
    }

    #[test]
    fn test_structured_error_has_required_fields() {
        // Verify structured rejection includes mutation_index, code, message, context.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease(
            "agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );

        let batch = make_batch(
            "agent",
            vec![
                SceneMutation::CreateTile {
                    tab_id,
                    namespace: "agent".to_string(),
                    lease_id,
                    bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                    z_order: 1,
                },
                // Second mutation fails with invalid bounds
                SceneMutation::UpdateTileBounds {
                    tile_id: SceneId::new(), // non-existent tile
                    bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                },
            ],
        );

        let result = scene.apply_batch(&batch);
        assert!(!result.applied);
        let rej = result.rejection.unwrap();
        let err = &rej.errors[0];
        assert_eq!(err.mutation_index, Some(1));
        assert_eq!(err.mutation_type, "UpdateTileBounds");
        assert!(!err.message.is_empty());
        // Context must be a JSON object
        assert!(err.context.is_object());
    }
}
