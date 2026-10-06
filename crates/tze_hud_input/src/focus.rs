//! Focus manager per RFC 0004 §1.1–§1.4, §5.6.
//!
//! `FocusManager` is the authoritative runtime component for focus state. It:
//! - Maintains one `FocusTree` per tab.
//! - Enforces the single-focus-owner-per-tab invariant.
//! - Handles click-to-focus, programmatic requests, cycling, and destruction
//!   fallback.
//! - Emits `FocusGainedEvent` and `FocusLostEvent` as plain Rust values; callers
//!   (the input processor / runtime kernel) are responsible for routing them to
//!   the owning agent via gRPC/session.
//! - Provides focus ring metadata (node bounds) for the compositor.
//! - Enforces focus isolation: queries are filtered by ownership.
//!
//! Spec refs:
//! - Lines 11-13:  Focus Tree Structure
//! - Lines 27-29:  Click-to-Focus Acquisition
//! - Lines 42-44:  Programmatic Focus Request
//! - Lines 57-58:  Focus Transfer on Destruction
//! - Lines 67-69:  Focus Isolation Between Agents
//! - Lines 79-81:  Focus Cycling
//! - Lines 93-94:  Focus Events Dispatch
//! - Lines 400-402: Focus Ring Visual Indication

use std::collections::HashMap;
use tze_hud_scene::{InputMode, NodeData, SceneGraph, SceneId};

use crate::focus_tree::{FocusOwner, FocusTree};

// ─── Focus events ─────────────────────────────────────────────────────────────

/// Why focus was gained (RFC 0004 §1.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusSource {
    /// Focus acquired by a pointer click.
    Click,
    /// Focus advanced via Tab / NAVIGATE_NEXT / NAVIGATE_PREV key.
    TabKey,
    /// Focus set by an explicit programmatic `FocusRequest`.
    Programmatic,
    /// Focus moved by a CommandInputEvent (e.g. D-pad NAVIGATE_NEXT).
    CommandInput,
}

/// Why focus was lost (RFC 0004 §1.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusLostReason {
    /// Pointer clicked somewhere else.
    ClickElsewhere,
    /// Tab / NAVIGATE_PREV/NEXT moved focus away.
    TabKey,
    /// Programmatic request transferred focus.
    Programmatic,
    /// The focused tile or node was destroyed.
    TileDestroyed,
    /// The tab containing the focused element was switched away from.
    TabSwitched,
    /// The lease held by the agent was revoked.
    LeaseRevoked,
    /// The agent disconnected.
    AgentDisconnected,
    /// A CommandInputEvent moved focus.
    CommandInput,
}

/// Emitted when a tile or node gains focus.
///
/// The owning agent should receive this event before any triggering pointer
/// event (spec line 32).
#[derive(Clone, Debug)]
pub struct FocusGainedEvent {
    pub tile_id: SceneId,
    /// None for tile-level focus.
    pub node_id: Option<SceneId>,
    pub source: FocusSource,
}

/// Emitted when a tile or node loses focus.
#[derive(Clone, Debug)]
pub struct FocusLostEvent {
    pub tile_id: SceneId,
    /// None for tile-level focus.
    pub node_id: Option<SceneId>,
    pub reason: FocusLostReason,
}

// ─── Programmatic request / response ─────────────────────────────────────────

/// Agent request to programmatically acquire focus (RFC 0004 §1.2).
#[derive(Clone, Debug)]
pub struct FocusRequest {
    /// The tile the agent owns.
    pub tile_id: SceneId,
    /// The specific node, or None for tile-level focus.
    pub node_id: Option<SceneId>,
    /// If true, steal focus even if another agent holds it.
    /// The runtime MAY still deny if the current owner has an active interaction.
    pub steal: bool,
    /// The namespace (agent name) making the request, for ownership validation.
    pub requesting_namespace: String,
}

/// Runtime response to a `FocusRequest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusResult {
    /// Focus was granted.
    Granted,
    /// Request denied: another agent holds focus and `steal=false`.
    Denied,
    /// Request denied: tile/node does not exist or is not owned by requester.
    Invalid,
}

// ─── Focus ring metadata (compositor surface) ─────────────────────────────────

/// Bounds and style metadata for focus ring rendering (RFC 0004 §5.6).
///
/// The compositor renders this in the chrome layer, above all agent content.
/// Default style: 2px solid ring, system accent color, ≥3:1 contrast ratio.
#[derive(Clone, Debug)]
pub struct FocusRingUpdate {
    /// Tab on which the ring should be rendered.
    pub tab_id: SceneId,
    /// Display-space bounding rectangle of the focused element.
    /// None means the ring should be cleared (focus is None or ChromeElement).
    pub bounds: Option<FocusRingBounds>,
}

#[derive(Clone, Debug)]
pub struct FocusRingBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

// ─── Focus transition outcome ─────────────────────────────────────────────────

/// All side effects produced by a focus transition.
///
/// Callers unpack this and route events to the appropriate agents.
#[derive(Clone, Debug, Default)]
pub struct FocusTransition {
    /// Event to dispatch to the agent that *lost* focus (if any).
    pub lost: Option<(FocusLostEvent, String)>, // (event, namespace)
    /// Event to dispatch to the agent that *gained* focus (if any).
    pub gained: Option<(FocusGainedEvent, String)>, // (event, namespace)
    /// Updated focus ring metadata for the compositor.
    pub ring_update: Option<FocusRingUpdate>,
}

// ─── Focus manager ─────────────────────────────────────────────────────────────

/// Per-runtime focus manager. One instance per runtime (global across all tabs).
///
/// Focus state is per-tab; this manager holds one `FocusTree` per tab.
/// The active tab is taken from `SceneGraph::active_tab`.
pub struct FocusManager {
    /// Per-tab focus trees, keyed by tab_id.
    trees: HashMap<SceneId, FocusTree>,
}

impl FocusManager {
    pub fn new() -> Self {
        Self {
            trees: HashMap::new(),
        }
    }

    // ─── Tab lifecycle ──────────────────────────────────────────────────

    /// Register a new tab. Called when a tab is created in the scene graph.
    pub fn add_tab(&mut self, tab_id: SceneId) {
        self.trees.entry(tab_id).or_default();
    }

    /// Remove a tab's focus tree. Called when a tab is destroyed.
    pub fn remove_tab(&mut self, tab_id: SceneId) {
        self.trees.remove(&tab_id);
    }

    // ─── Tab switch ─────────────────────────────────────────────────────

    /// Called when the user switches from `from_tab` to `to_tab`.
    ///
    /// Per spec lines 21-22: focus state is preserved on suspension without
    /// emitting FocusLostEvent on the suspended tab. The target tab's existing
    /// focus is restored without emitting FocusGainedEvent.
    ///
    /// Returns a `FocusRingUpdate` for the newly active tab (compositor needs
    /// to update the ring immediately after switch).
    pub fn on_tab_switch(
        &mut self,
        _from_tab: SceneId,
        to_tab: SceneId,
        scene: &SceneGraph,
    ) -> FocusRingUpdate {
        // No events emitted; just return ring metadata for new active tab.
        self.compute_ring_update(to_tab, scene)
    }

    // ─── Click-to-focus (spec lines 27-29) ─────────────────────────────

    /// Called when a pointer down event hits a node or tile.
    ///
    /// Transfers focus to the hit target (before the pointer event is dispatched)
    /// unless the tile is Passthrough. Returns the focus transition side-effects.
    pub fn on_click(
        &mut self,
        tab_id: SceneId,
        tile_id: SceneId,
        node_id: Option<SceneId>,
        scene: &SceneGraph,
    ) -> FocusTransition {
        // Validate tile exists and belongs to this tab (prevents cross-tab focus corruption).
        if let Some(tile) = scene.tiles.get(&tile_id) {
            if tile.tab_id != tab_id {
                return FocusTransition::default();
            }
            // Passthrough tiles do not acquire focus (spec line 37).
            if tile.input_mode == InputMode::Passthrough {
                return FocusTransition::default();
            }
        } else {
            return FocusTransition::default();
        }

        // Determine new owner.
        let new_owner = match node_id {
            Some(nid) => {
                // Only accept nodes with accepts_focus=true.
                if let Some(node) = scene.nodes.get(&nid) {
                    if let NodeData::HitRegion(hr) = &node.data {
                        if hr.accepts_focus {
                            FocusOwner::Node {
                                tile_id,
                                node_id: nid,
                            }
                        } else {
                            // accepts_focus=false — fall back to tile-level.
                            FocusOwner::Tile(tile_id)
                        }
                    } else {
                        FocusOwner::Tile(tile_id)
                    }
                } else {
                    FocusOwner::Tile(tile_id)
                }
            }
            None => FocusOwner::Tile(tile_id),
        };

        self.apply_transition(
            tab_id,
            new_owner,
            FocusSource::Click,
            FocusLostReason::ClickElsewhere,
            scene,
        )
    }

    // ─── Programmatic focus request (spec lines 42-44) ──────────────────

    /// Handle a programmatic focus request from an agent.
    ///
    /// Returns `(FocusResult, FocusTransition)`.
    pub fn request_focus(
        &mut self,
        req: FocusRequest,
        tab_id: SceneId,
        scene: &SceneGraph,
    ) -> (FocusResult, FocusTransition) {
        // Validate ownership: tile must exist and belong to requesting namespace.
        let tile = match scene.tiles.get(&req.tile_id) {
            Some(t) => t,
            None => return (FocusResult::Invalid, FocusTransition::default()),
        };
        if tile.namespace != req.requesting_namespace {
            return (FocusResult::Invalid, FocusTransition::default());
        }
        // Ensure the tile belongs to the given tab; prevents cross-tab focus mutation.
        if tile.tab_id != tab_id {
            return (FocusResult::Invalid, FocusTransition::default());
        }

        // Validate node if provided: must exist, be a focusable HitRegion,
        // and be reachable from this tile's root (prevents cross-tile node injection).
        if let Some(nid) = req.node_id {
            match scene.nodes.get(&nid) {
                Some(node) => {
                    if let NodeData::HitRegion(hr) = &node.data {
                        if !hr.accepts_focus {
                            return (FocusResult::Invalid, FocusTransition::default());
                        }
                    } else {
                        return (FocusResult::Invalid, FocusTransition::default());
                    }
                }
                None => return (FocusResult::Invalid, FocusTransition::default()),
            }
            // Verify the node is reachable from this tile's root tree.
            let tile = scene.tiles.get(&req.tile_id).unwrap(); // validated above
            let reachable = collect_focusable_nodes(tile.root_node, &scene.nodes).contains(&nid)
                || {
                    // collect_focusable_nodes only returns accepts_focus nodes;
                    // do a full DFS to check reachability regardless of accepts_focus.
                    let mut visited = std::collections::HashSet::new();
                    node_reachable_from(tile.root_node, nid, &scene.nodes, &mut visited)
                };
            if !reachable {
                return (FocusResult::Invalid, FocusTransition::default());
            }
        }

        // Check steal semantics.
        let tree = self.tree_for(tab_id);
        let another_holds_focus = match tree.current() {
            FocusOwner::None => false,
            FocusOwner::ChromeElement(_) => true,
            FocusOwner::Tile(tid) => *tid != req.tile_id,
            FocusOwner::Node { tile_id: tid, .. } => *tid != req.tile_id,
        };

        if another_holds_focus && !req.steal {
            return (FocusResult::Denied, FocusTransition::default());
        }

        // Granted — apply transition.
        let new_owner = match req.node_id {
            Some(nid) => FocusOwner::Node {
                tile_id: req.tile_id,
                node_id: nid,
            },
            None => FocusOwner::Tile(req.tile_id),
        };
        let transition = self.apply_transition(
            tab_id,
            new_owner,
            FocusSource::Programmatic,
            FocusLostReason::Programmatic,
            scene,
        );
        (FocusResult::Granted, transition)
    }

    // ─── Focus cycling (spec lines 79-81) ───────────────────────────────

    /// Advance focus to the next focusable element in z-order (NAVIGATE_NEXT).
    ///
    /// Traversal: tiles sorted by z_order ascending, within each tile
    /// depth-first left-to-right tree order. Passthrough tiles skipped.
    /// Non-passthrough tiles with no focusable nodes receive tile-level focus.
    /// Wraps at end.
    pub fn navigate_next(&mut self, tab_id: SceneId, scene: &SceneGraph) -> FocusTransition {
        self.navigate(tab_id, scene, false)
    }

    /// Move focus to the previous focusable element (NAVIGATE_PREV).
    pub fn navigate_prev(&mut self, tab_id: SceneId, scene: &SceneGraph) -> FocusTransition {
        self.navigate(tab_id, scene, true)
    }

    fn navigate(&mut self, tab_id: SceneId, scene: &SceneGraph, reverse: bool) -> FocusTransition {
        let cycle = build_focus_cycle(tab_id, scene);
        if cycle.is_empty() {
            return FocusTransition::default();
        }

        let current = self.tree_for(tab_id).current().clone();
        let new_owner = advance_in_cycle(&cycle, &current, reverse);

        self.apply_transition(
            tab_id,
            new_owner,
            FocusSource::TabKey,
            FocusLostReason::TabKey,
            scene,
        )
    }

    /// Move focus directly to a specific node via a runtime-initiated command
    /// (source [`FocusSource::CommandInput`]).
    ///
    /// Used by the keyboard typing-recovery path (hud-2v8br): when the user types
    /// while a non-composer portal control holds focus, the runtime redirects
    /// focus to the tile's composer so the keystroke lands in the input box
    /// rather than being swallowed. Unlike [`Self::request_focus`] this bypasses
    /// namespace/steal validation because it is a runtime-owned recovery action,
    /// not an agent request; the caller is responsible for passing a node that
    /// belongs to `tile_id` on `tab_id`.
    ///
    /// Returns the [`FocusTransition`] so the caller can broadcast the
    /// gained/lost events exactly as the Tab / click paths do. A no-op transition
    /// is returned when focus already rests on the target node.
    pub fn focus_node_via_command(
        &mut self,
        tab_id: SceneId,
        tile_id: SceneId,
        node_id: SceneId,
        scene: &SceneGraph,
    ) -> FocusTransition {
        self.apply_transition(
            tab_id,
            FocusOwner::Node { tile_id, node_id },
            FocusSource::CommandInput,
            FocusLostReason::CommandInput,
            scene,
        )
    }

    /// Clear keyboard focus on `tab_id` to [`FocusOwner::None`] via a
    /// runtime-initiated command (hud-k6yvb).
    ///
    /// This is the Escape recovery for a focus stop that has no composer to fall
    /// back into (a tile-level stop, or a focusable node whose tile carries no
    /// composer): rather than stranding focus on a control the keyboard user
    /// cannot type into, Escape releases focus entirely. A subsequent Tab
    /// re-enters the cycle at the first stop (`navigate_next` from `None`), so
    /// this is never a dead end.
    ///
    /// Emits a `FocusLostEvent` for the released owner and a ring-clearing update.
    /// Returns a no-op transition when focus is already `None`/chrome.
    pub fn clear_focus(&mut self, tab_id: SceneId, scene: &SceneGraph) -> FocusTransition {
        let old_owner = self.tree_for(tab_id).current().clone();
        if matches!(old_owner, FocusOwner::None | FocusOwner::ChromeElement(_)) {
            return FocusTransition::default();
        }
        let lost = build_lost_event(&old_owner, FocusLostReason::CommandInput, scene);
        self.tree_for(tab_id).set_focus(FocusOwner::None);
        let ring_update = Some(self.compute_ring_update(tab_id, scene));
        FocusTransition {
            lost,
            gained: None,
            ring_update,
        }
    }

    // ─── Destruction fallback (spec lines 57-58) ────────────────────────

    /// Called when a tile is destroyed. If the destroyed tile holds focus,
    /// focus falls back to the previously focused element.
    ///
    /// **MUST be called before the tile is removed from `scene.tiles`** so that
    /// the tile's namespace can be looked up for `FocusLostEvent` dispatch.
    /// If called after removal the `FocusLostEvent` will be silently dropped.
    ///
    /// Returns the focus transition (may be empty if tile did not hold focus).
    pub fn on_tile_destroyed(
        &mut self,
        tab_id: SceneId,
        tile_id: SceneId,
        scene: &SceneGraph,
    ) -> FocusTransition {
        let was_focused = {
            let tree = self.tree_for(tab_id);
            tree.current().is_on_tile(tile_id)
        };

        if !was_focused {
            // Clean up any history entries for this tile.
            let tree = self.tree_for(tab_id);
            tree.remove_from_history(tile_id);
            return FocusTransition::default();
        }

        // Grab the current owner info before we mutate.
        let lost_tile = tile_id;
        let lost_node = {
            let tree = self.tree_for(tab_id);
            match tree.current() {
                FocusOwner::Node { node_id, .. } => Some(*node_id),
                _ => None,
            }
        };

        // Fallback to previous owner.
        let fallback = {
            let tree = self.tree_for(tab_id);
            tree.pop_fallback(tile_id)
        };
        // Also remove any remaining history entries for this tile.
        let tree = self.tree_for(tab_id);
        tree.remove_from_history(tile_id);
        // Set current to fallback (without pushing to history since we already
        // popped it from history).
        tree.current = fallback.clone();

        // Build lost event for the destroyed tile's owner.
        let mut transition = FocusTransition::default();
        let namespace = scene.tiles.get(&lost_tile).map(|t| t.namespace.clone());
        if let Some(ns) = namespace {
            transition.lost = Some((
                FocusLostEvent {
                    tile_id: lost_tile,
                    node_id: lost_node,
                    reason: FocusLostReason::TileDestroyed,
                },
                ns,
            ));
        }

        // Build gained event if fallback is a real tile/node.
        transition.gained = build_gained_event(&fallback, FocusSource::Programmatic, scene);

        transition.ring_update = Some(self.compute_ring_update(tab_id, scene));
        transition
    }

    /// Clears focus from a tile that no longer exists in the scene.
    ///
    /// Lease revocation and expiry remove a tile without telling the focus
    /// manager, so the runtime calls this every frame. Focus falls back through
    /// history, skipping tiles that are also gone. The `FocusLostEvent` is
    /// dropped: the owning agent's tile is already gone.
    pub fn clear_focus_on_missing_tile(
        &mut self,
        tab_id: SceneId,
        scene: &SceneGraph,
    ) -> FocusTransition {
        let mut transition = FocusTransition::default();
        while let Some(tile_id) = self.current_owner(tab_id).tile_id() {
            if scene.tiles.contains_key(&tile_id) {
                break;
            }
            transition = self.on_tile_destroyed(tab_id, tile_id, scene);
        }
        transition
    }

    // ─── Focus isolation (spec lines 67-69) ─────────────────────────────

    /// Returns the current focus owner visible to `namespace`.
    ///
    /// Returns `None` if the focused element is not owned by `namespace`
    /// (enforcing focus isolation between agents).
    pub fn current_focus_for_namespace(
        &self,
        tab_id: SceneId,
        namespace: &str,
        scene: &SceneGraph,
    ) -> Option<&FocusOwner> {
        let tree = self.trees.get(&tab_id)?;
        let owner = tree.current();
        match owner {
            FocusOwner::None => None,
            FocusOwner::ChromeElement(_) => None,
            FocusOwner::Tile(tid) => {
                let tile = scene.tiles.get(tid)?;
                if tile.namespace == namespace {
                    Some(owner)
                } else {
                    None
                }
            }
            FocusOwner::Node { tile_id, .. } => {
                let tile = scene.tiles.get(tile_id)?;
                if tile.namespace == namespace {
                    Some(owner)
                } else {
                    None
                }
            }
        }
    }

    // ─── Public accessors ────────────────────────────────────────────────

    /// Read-only access to all focus trees, keyed by tab_id.
    ///
    /// Provided for integration points (e.g. `InputProcessor::process_with_focus`)
    /// that need to inspect current focus state without going through the lifecycle
    /// API.
    pub fn trees(&self) -> &HashMap<SceneId, FocusTree> {
        &self.trees
    }

    /// Returns the current focus owner for the given tab, or `FocusOwner::None`
    /// if the tab has no registered tree.
    ///
    /// Convenience wrapper over `trees().get(tab_id).map(|t| t.current())`.
    /// Used by the runtime keyboard drain path to route raw key events to the
    /// correct focused node without going through the full lifecycle API.
    pub fn current_owner(&self, tab_id: SceneId) -> &FocusOwner {
        self.trees
            .get(&tab_id)
            .map(|t| t.current())
            .unwrap_or(&FocusOwner::None)
    }

    // ─── Internal helpers ────────────────────────────────────────────────

    /// Get-or-create the focus tree for a tab.
    fn tree_for(&mut self, tab_id: SceneId) -> &mut FocusTree {
        self.trees.entry(tab_id).or_default()
    }

    /// Apply a focus transition: update the tree and emit events.
    fn apply_transition(
        &mut self,
        tab_id: SceneId,
        new_owner: FocusOwner,
        gained_source: FocusSource,
        lost_reason: FocusLostReason,
        scene: &SceneGraph,
    ) -> FocusTransition {
        let tree = self.tree_for(tab_id);
        let old_owner = tree.current().clone();

        // No-op if focus is already on the same element.
        if old_owner == new_owner {
            return FocusTransition::default();
        }

        // Build lost event before transitioning.
        let lost = build_lost_event(&old_owner, lost_reason, scene);

        // Transition.
        tree.set_focus(new_owner.clone());

        // Build gained event.
        let gained = build_gained_event(&new_owner, gained_source, scene);

        // Focus ring update.
        let ring_update = Some(compute_ring(tab_id, &new_owner, scene));

        FocusTransition {
            lost,
            gained,
            ring_update,
        }
    }

    /// Compute focus ring update for the given tab (used for tab switch / restore).
    fn compute_ring_update(&self, tab_id: SceneId, scene: &SceneGraph) -> FocusRingUpdate {
        let owner = self
            .trees
            .get(&tab_id)
            .map(|t| t.current())
            .unwrap_or(&FocusOwner::None);
        compute_ring(tab_id, owner, scene)
    }
}

impl Default for FocusManager {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Focus cycle helpers ──────────────────────────────────────────────────────

/// A single step in the focus cycle (a focusable element in traversal order).
#[derive(Clone, Debug, PartialEq, Eq)]
enum CycleStep {
    /// A specific HitRegionNode.
    Node { tile_id: SceneId, node_id: SceneId },
    /// A non-passthrough tile with no focusable nodes (receives tile-level focus).
    TileLevel(SceneId),
}

impl CycleStep {
    fn to_focus_owner(&self) -> FocusOwner {
        match self {
            CycleStep::Node { tile_id, node_id } => FocusOwner::Node {
                tile_id: *tile_id,
                node_id: *node_id,
            },
            CycleStep::TileLevel(id) => FocusOwner::Tile(*id),
        }
    }

    fn matches(&self, owner: &FocusOwner) -> bool {
        match (self, owner) {
            (
                CycleStep::Node {
                    tile_id: t1,
                    node_id: n1,
                },
                FocusOwner::Node {
                    tile_id: t2,
                    node_id: n2,
                },
            ) => t1 == t2 && n1 == n2,
            (CycleStep::TileLevel(a), FocusOwner::Tile(b)) => a == b,
            _ => false,
        }
    }
}

/// Build the ordered focus cycle for a tab (spec lines 79-81).
///
/// Order: tiles by z_order ascending (lowest first), within each tile
/// depth-first left-to-right tree order of HitRegionNodes with accepts_focus=true.
/// Passthrough tiles excluded. Chrome tab bar excluded (it is a chrome element,
/// not in the tiles map).
fn build_focus_cycle(tab_id: SceneId, scene: &SceneGraph) -> Vec<CycleStep> {
    // Collect tiles that belong to this tab, sorted by z_order ascending.
    let mut tiles: Vec<_> = scene
        .tiles
        .values()
        .filter(|t| t.tab_id == tab_id && t.input_mode != InputMode::Passthrough)
        .collect();
    tiles.sort_by_key(|t| t.z_order);

    let mut cycle = Vec::new();
    for tile in tiles {
        let focusable_nodes = collect_focusable_nodes(tile.root_node, &scene.nodes);
        if focusable_nodes.is_empty() {
            // Non-passthrough tile with no focusable nodes → tile-level step.
            cycle.push(CycleStep::TileLevel(tile.id));
        } else {
            for node_id in focusable_nodes {
                cycle.push(CycleStep::Node {
                    tile_id: tile.id,
                    node_id,
                });
            }
        }
    }
    cycle
}

/// Collect focusable HitRegionNode IDs in depth-first left-to-right order.
fn collect_focusable_nodes(
    root: Option<SceneId>,
    nodes: &std::collections::HashMap<SceneId, tze_hud_scene::types::Node>,
) -> Vec<SceneId> {
    let mut result = Vec::new();
    if let Some(root_id) = root {
        collect_dfs(root_id, nodes, &mut result);
    }
    result
}

fn collect_dfs(
    node_id: SceneId,
    nodes: &std::collections::HashMap<SceneId, tze_hud_scene::types::Node>,
    result: &mut Vec<SceneId>,
) {
    let node = match nodes.get(&node_id) {
        Some(n) => n,
        None => return,
    };
    if let NodeData::HitRegion(hr) = &node.data
        && hr.accepts_focus
    {
        result.push(node_id);
    }
    for child in &node.children {
        collect_dfs(*child, nodes, result);
    }
}

/// Check whether `target` is reachable from `root` in the node tree.
///
/// Used by `request_focus` to reject node IDs that are not part of the
/// requested tile's node tree, preventing cross-tile node injection.
fn node_reachable_from(
    root: Option<SceneId>,
    target: SceneId,
    nodes: &std::collections::HashMap<SceneId, tze_hud_scene::types::Node>,
    visited: &mut std::collections::HashSet<SceneId>,
) -> bool {
    let root_id = match root {
        Some(id) => id,
        None => return false,
    };
    if root_id == target {
        return true;
    }
    if !visited.insert(root_id) {
        return false; // cycle guard
    }
    let node = match nodes.get(&root_id) {
        Some(n) => n,
        None => return false,
    };
    node.children
        .iter()
        .any(|&child| node_reachable_from(Some(child), target, nodes, visited))
}

/// Advance to the next/previous step in the cycle.
fn advance_in_cycle(cycle: &[CycleStep], current: &FocusOwner, reverse: bool) -> FocusOwner {
    if cycle.is_empty() {
        return FocusOwner::None;
    }

    // Find current index.
    let current_idx = cycle.iter().position(|s| s.matches(current));

    let next_idx = match current_idx {
        None => {
            // No current focus or not in cycle — start at beginning (or end if reverse).
            if reverse { cycle.len() - 1 } else { 0 }
        }
        Some(idx) => {
            if reverse {
                if idx == 0 { cycle.len() - 1 } else { idx - 1 }
            } else {
                (idx + 1) % cycle.len()
            }
        }
    };

    cycle[next_idx].to_focus_owner()
}

// ─── Event builders ──────────────────────────────────────────────────────────

fn build_lost_event(
    old: &FocusOwner,
    reason: FocusLostReason,
    scene: &SceneGraph,
) -> Option<(FocusLostEvent, String)> {
    match old {
        FocusOwner::Tile(tid) => {
            let ns = scene.tiles.get(tid).map(|t| t.namespace.clone())?;
            Some((
                FocusLostEvent {
                    tile_id: *tid,
                    node_id: None,
                    reason,
                },
                ns,
            ))
        }
        FocusOwner::Node { tile_id, node_id } => {
            let ns = scene.tiles.get(tile_id).map(|t| t.namespace.clone())?;
            Some((
                FocusLostEvent {
                    tile_id: *tile_id,
                    node_id: Some(*node_id),
                    reason,
                },
                ns,
            ))
        }
        // ChromeElement and None: no agent notification.
        _ => None,
    }
}

fn build_gained_event(
    new: &FocusOwner,
    source: FocusSource,
    scene: &SceneGraph,
) -> Option<(FocusGainedEvent, String)> {
    match new {
        FocusOwner::Tile(tid) => {
            let ns = scene.tiles.get(tid).map(|t| t.namespace.clone())?;
            Some((
                FocusGainedEvent {
                    tile_id: *tid,
                    node_id: None,
                    source,
                },
                ns,
            ))
        }
        FocusOwner::Node { tile_id, node_id } => {
            let ns = scene.tiles.get(tile_id).map(|t| t.namespace.clone())?;
            Some((
                FocusGainedEvent {
                    tile_id: *tile_id,
                    node_id: Some(*node_id),
                    source,
                },
                ns,
            ))
        }
        _ => None,
    }
}

fn compute_ring(tab_id: SceneId, owner: &FocusOwner, scene: &SceneGraph) -> FocusRingUpdate {
    let bounds = match owner {
        FocusOwner::Node { tile_id, node_id } => {
            scene.tiles.get(tile_id).and_then(|tile| {
                scene.nodes.get(node_id).and_then(|node| {
                    if let NodeData::HitRegion(hr) = &node.data {
                        // Convert tile-local node bounds to display-space.
                        Some(FocusRingBounds {
                            x: tile.bounds.x + hr.bounds.x,
                            y: tile.bounds.y + hr.bounds.y,
                            width: hr.bounds.width,
                            height: hr.bounds.height,
                        })
                    } else {
                        None
                    }
                })
            })
        }
        FocusOwner::Tile(tid) => scene.tiles.get(tid).map(|tile| FocusRingBounds {
            x: tile.bounds.x,
            y: tile.bounds.y,
            width: tile.bounds.width,
            height: tile.bounds.height,
        }),
        // No ring for None or ChromeElement (chrome handles its own highlight).
        _ => None,
    };
    FocusRingUpdate { tab_id, bounds }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::{HitRegionNode, Node, NodeData, Rect, SceneGraph, SceneId};

    // ── Scene setup helpers ──────────────────────────────────────────────

    fn setup_scene() -> (SceneGraph, SceneId, SceneId) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent-a", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent-a",
                lease_id,
                Rect::new(100.0, 100.0, 400.0, 300.0),
                1,
            )
            .unwrap();
        (scene, tab_id, tile_id)
    }

    fn add_hit_region(
        scene: &mut SceneGraph,
        tile_id: SceneId,
        bounds: Rect,
        interaction_id: &str,
        accepts_focus: bool,
    ) -> SceneId {
        let node_id = SceneId::new();
        let node = Node {
            layout: Default::default(),
            id: node_id,
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds,
                interaction_id: interaction_id.to_string(),
                accepts_focus,
                accepts_pointer: true,
                ..Default::default()
            }),
        };
        scene.set_tile_root(tile_id, node).unwrap();
        node_id
    }

    // ── Escape recovery: clear-to-no-focus then Tab re-enters (hud-k6yvb) ──

    #[test]
    fn clear_focus_releases_and_tab_reenters_cycle() {
        // A composer-less tile with one focusable node — the case #988 could
        // strand: no composer to recover into.
        let (mut scene, tab_id, tile_id) = setup_scene();
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "control",
            true,
        );
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Land focus on the control.
        fm.navigate_next(tab_id, &scene);
        assert_eq!(fm.current_owner(tab_id).node_id(), Some(node_id));

        // Escape clears focus entirely (no composer to fall back into).
        let cleared = fm.clear_focus(tab_id, &scene);
        assert!(
            cleared.lost.is_some(),
            "clearing focus must emit a FocusLostEvent for the released control"
        );
        assert_eq!(
            *fm.current_owner(tab_id),
            FocusOwner::None,
            "Escape on a composer-less stop must clear focus to None"
        );
        // Ring update clears (no bounds) since focus is None.
        assert!(
            cleared
                .ring_update
                .as_ref()
                .map(|r| r.bounds.is_none())
                .unwrap_or(false),
            "cleared focus must clear the ring"
        );

        // Tab re-enters the cycle at the first stop — never a dead end.
        fm.navigate_next(tab_id, &scene);
        assert_eq!(
            fm.current_owner(tab_id).node_id(),
            Some(node_id),
            "Tab after clear must re-enter the cycle at the first stop"
        );
    }

    #[test]
    fn clear_focus_is_noop_when_already_unfocused() {
        let (scene, tab_id, _tile_id) = setup_scene();
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);
        let t = fm.clear_focus(tab_id, &scene);
        assert!(t.lost.is_none() && t.gained.is_none());
    }

    // ── Invariant: single focus owner per tab ────────────────────────────

    #[test]
    fn test_single_focus_owner_per_tab() {
        let (mut scene, tab_id, tile_id) = setup_scene();
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn",
            true,
        );

        let lease_id2 = scene.grant_lease("agent-b", 60_000);
        let tile_id2 = scene
            .create_tile(
                tab_id,
                "agent-b",
                lease_id2,
                Rect::new(200.0, 200.0, 100.0, 100.0),
                2,
            )
            .unwrap();
        let node_id2 = add_hit_region(
            &mut scene,
            tile_id2,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn2",
            true,
        );

        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Click on node in tile 1.
        fm.on_click(tab_id, tile_id, Some(node_id), &scene);
        // Current focus must be tile1/node1.
        let owner = fm.trees[&tab_id].current().clone();
        assert_eq!(owner, FocusOwner::Node { tile_id, node_id });

        // Click on node in tile 2.
        fm.on_click(tab_id, tile_id2, Some(node_id2), &scene);
        // Now focus must be tile2/node2 only.
        let owner2 = fm.trees[&tab_id].current().clone();
        assert_eq!(
            owner2,
            FocusOwner::Node {
                tile_id: tile_id2,
                node_id: node_id2
            }
        );
        // Ensure old is no longer focused in local state.
        assert_ne!(owner2, FocusOwner::Node { tile_id, node_id });
    }

    // ── Click-to-focus ───────────────────────────────────────────────────

    #[test]
    fn test_click_on_focusable_node_transfers_focus() {
        let (mut scene, tab_id, tile_id) = setup_scene();
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn",
            true,
        );
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        let t = fm.on_click(tab_id, tile_id, Some(node_id), &scene);

        let owner = fm.trees[&tab_id].current().clone();
        assert_eq!(owner, FocusOwner::Node { tile_id, node_id });
        assert!(t.gained.is_some(), "should emit FocusGainedEvent");
        let (ev, ns) = t.gained.unwrap();
        assert_eq!(ev.source, FocusSource::Click);
        assert_eq!(ev.tile_id, tile_id);
        assert_eq!(ev.node_id, Some(node_id));
        assert_eq!(ns, "agent-a");
    }

    #[test]
    fn test_click_on_passthrough_tile_does_not_change_focus() {
        let (mut scene, tab_id, tile_id) = setup_scene();

        // Make the tile passthrough.
        scene.tiles.get_mut(&tile_id).unwrap().input_mode = InputMode::Passthrough;

        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn",
            true,
        );
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        let prev_owner = fm.trees[&tab_id].current().clone();
        let t = fm.on_click(tab_id, tile_id, Some(node_id), &scene);
        let new_owner = fm.trees[&tab_id].current().clone();

        assert_eq!(
            prev_owner, new_owner,
            "focus must not change for passthrough tile"
        );
        assert!(t.gained.is_none());
        assert!(t.lost.is_none());
    }

    // ── Programmatic focus ───────────────────────────────────────────────

    #[test]
    fn test_programmatic_focus_granted_when_no_holder() {
        let (mut scene, tab_id, tile_id) = setup_scene();
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn",
            true,
        );
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        let req = FocusRequest {
            tile_id,
            node_id: Some(node_id),
            steal: false,
            requesting_namespace: "agent-a".into(),
        };
        let (result, t) = fm.request_focus(req, tab_id, &scene);

        assert_eq!(result, FocusResult::Granted);
        assert!(t.gained.is_some());
        let (ev, _) = t.gained.unwrap();
        assert_eq!(ev.source, FocusSource::Programmatic);
    }

    #[test]
    fn test_programmatic_focus_denied_when_another_holds_and_no_steal() {
        let (mut scene, tab_id, tile_id) = setup_scene();
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn",
            true,
        );

        // Create a second tile owned by agent-b.
        let lease_id2 = scene.grant_lease("agent-b", 60_000);
        let tile_id2 = scene
            .create_tile(
                tab_id,
                "agent-b",
                lease_id2,
                Rect::new(300.0, 300.0, 100.0, 100.0),
                2,
            )
            .unwrap();
        let node_id2 = add_hit_region(
            &mut scene,
            tile_id2,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn2",
            true,
        );

        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Give agent-a focus.
        fm.on_click(tab_id, tile_id, Some(node_id), &scene);

        // agent-b tries steal=false — should be denied.
        let req = FocusRequest {
            tile_id: tile_id2,
            node_id: Some(node_id2),
            steal: false,
            requesting_namespace: "agent-b".into(),
        };
        let (result, _) = fm.request_focus(req, tab_id, &scene);
        assert_eq!(result, FocusResult::Denied);
    }

    #[test]
    fn test_programmatic_focus_invalid_wrong_namespace() {
        let (mut scene, tab_id, tile_id) = setup_scene();
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn",
            true,
        );
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // agent-b tries to request focus on agent-a's tile.
        let req = FocusRequest {
            tile_id,
            node_id: Some(node_id),
            steal: false,
            requesting_namespace: "agent-b".into(),
        };
        let (result, _) = fm.request_focus(req, tab_id, &scene);
        assert_eq!(result, FocusResult::Invalid);
    }

    // ── Focus transfer on destruction ────────────────────────────────────

    #[test]
    fn test_focus_fallback_on_tile_destruction() {
        let (mut scene, tab_id, tile_id) = setup_scene();
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn",
            true,
        );

        // Create a second tile.
        let lease_id2 = scene.grant_lease("agent-b", 60_000);
        let tile_id2 = scene
            .create_tile(
                tab_id,
                "agent-b",
                lease_id2,
                Rect::new(300.0, 300.0, 100.0, 100.0),
                2,
            )
            .unwrap();
        let node_id2 = add_hit_region(
            &mut scene,
            tile_id2,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn2",
            true,
        );

        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Focus on tile2/node2 first.
        fm.on_click(tab_id, tile_id2, Some(node_id2), &scene);
        // Then focus on tile/node.
        fm.on_click(tab_id, tile_id, Some(node_id), &scene);

        // Destroy tile (focused element).
        let t = fm.on_tile_destroyed(tab_id, tile_id, &scene);

        // Focus should fall back to tile2/node2.
        let owner = fm.trees[&tab_id].current().clone();
        assert_eq!(
            owner,
            FocusOwner::Node {
                tile_id: tile_id2,
                node_id: node_id2
            }
        );
        // Lost event dispatched to agent-a.
        assert!(t.lost.is_some());
        let (ev, ns) = t.lost.unwrap();
        assert_eq!(ev.reason, FocusLostReason::TileDestroyed);
        assert_eq!(ns, "agent-a");
    }

    #[test]
    fn test_focus_falls_to_none_when_no_history() {
        let (mut scene, tab_id, tile_id) = setup_scene();
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn",
            true,
        );

        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);
        fm.on_click(tab_id, tile_id, Some(node_id), &scene);

        let t = fm.on_tile_destroyed(tab_id, tile_id, &scene);
        let owner = fm.trees[&tab_id].current().clone();
        assert_eq!(owner, FocusOwner::None);
        assert!(t.lost.is_some());
    }

    // ── Focus isolation ──────────────────────────────────────────────────

    #[test]
    fn test_agent_cannot_see_other_agents_focus() {
        let (mut scene, tab_id, tile_id) = setup_scene();
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn",
            true,
        );

        let lease_id2 = scene.grant_lease("agent-b", 60_000);
        let _tile_id2 = scene
            .create_tile(
                tab_id,
                "agent-b",
                lease_id2,
                Rect::new(300.0, 300.0, 100.0, 100.0),
                2,
            )
            .unwrap();

        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Focus on agent-a's tile.
        fm.on_click(tab_id, tile_id, Some(node_id), &scene);

        // agent-b queries — must see None.
        let visible = fm.current_focus_for_namespace(tab_id, "agent-b", &scene);
        assert!(visible.is_none(), "agent-b must not see agent-a's focus");

        // agent-a queries — should see their own focus.
        let visible_a = fm.current_focus_for_namespace(tab_id, "agent-a", &scene);
        assert!(visible_a.is_some());
    }

    // ── Focus cycling ────────────────────────────────────────────────────

    #[test]
    fn test_focus_cycling_z_order() {
        // Build scene: Tile z=1 with N1, N2; Tile z=3 with N3; Tile z=8 no focusable.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();

        let lease_a = scene.grant_lease("a", 60_000);
        let lease_b = scene.grant_lease("b", 60_000);
        let lease_c = scene.grant_lease("c", 60_000);

        let t1 = scene
            .create_tile(tab_id, "a", lease_a, Rect::new(0.0, 0.0, 100.0, 100.0), 1)
            .unwrap();
        let t3 = scene
            .create_tile(tab_id, "b", lease_b, Rect::new(0.0, 0.0, 100.0, 100.0), 3)
            .unwrap();
        let t8 = scene
            .create_tile(tab_id, "c", lease_c, Rect::new(0.0, 0.0, 100.0, 100.0), 8)
            .unwrap();

        // t1: N1, N2.
        let n1 = SceneId::new();
        let n2 = SceneId::new();
        let node_n1 = Node {
            layout: Default::default(),
            id: n1,
            children: vec![n2],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 0.0, 50.0, 50.0),
                interaction_id: "n1".into(),
                accepts_focus: true,
                accepts_pointer: true,
                ..Default::default()
            }),
        };
        let node_n2 = Node {
            layout: Default::default(),
            id: n2,
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 50.0, 50.0, 50.0),
                interaction_id: "n2".into(),
                accepts_focus: true,
                accepts_pointer: true,
                ..Default::default()
            }),
        };
        scene.nodes.insert(n1, node_n1);
        scene.nodes.insert(n2, node_n2);
        scene.tiles.get_mut(&t1).unwrap().root_node = Some(n1);

        // t3: N3.
        let n3 = SceneId::new();
        scene.nodes.insert(
            n3,
            Node {
                layout: Default::default(),
                id: n3,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 50.0, 50.0),
                    interaction_id: "n3".into(),
                    accepts_focus: true,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        );
        scene.tiles.get_mut(&t3).unwrap().root_node = Some(n3);

        // t8: no focusable nodes (has a SolidColor node).
        let nc = SceneId::new();
        scene.nodes.insert(
            nc,
            Node {
                layout: Default::default(),
                id: nc,
                children: vec![],
                data: NodeData::SolidColor(tze_hud_scene::SolidColorNode {
                    color: tze_hud_scene::Rgba::new(1.0, 0.0, 0.0, 1.0),
                    bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                    radius: None,
                }),
            },
        );
        scene.tiles.get_mut(&t8).unwrap().root_node = Some(nc);

        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Set initial focus to N2 (per scenario in spec line 84).
        fm.on_click(tab_id, t1, Some(n2), &scene);
        assert_eq!(
            fm.trees[&tab_id].current().clone(),
            FocusOwner::Node {
                tile_id: t1,
                node_id: n2
            }
        );

        // Tab → N3.
        fm.navigate_next(tab_id, &scene);
        assert_eq!(
            fm.trees[&tab_id].current().clone(),
            FocusOwner::Node {
                tile_id: t3,
                node_id: n3
            }
        );

        // Tab → T8 (tile-level focus, no focusable nodes).
        fm.navigate_next(tab_id, &scene);
        assert_eq!(fm.trees[&tab_id].current().clone(), FocusOwner::Tile(t8));

        // Tab → wrap to N1.
        fm.navigate_next(tab_id, &scene);
        assert_eq!(
            fm.trees[&tab_id].current().clone(),
            FocusOwner::Node {
                tile_id: t1,
                node_id: n1
            }
        );
    }

    /// hud-02sp5: a portal frame tile mixes genuinely-interactive controls
    /// (collapse / composer / reply) with pointer-only drag & resize handles.
    /// Tab traversal must stop on the interactive controls in depth-first
    /// left-to-right order, Shift+Tab must reverse that order, and the
    /// pointer-only handles must be skipped entirely.
    #[test]
    fn portal_tab_ring_visits_controls_and_skips_pointer_only_handles() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease = scene.grant_lease("portal-agent", 60_000);
        let tile = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease,
                Rect::new(100.0, 100.0, 400.0, 300.0),
                1,
            )
            .unwrap();

        // A portal frame: a SolidColor container root with affordance children
        // in render order, matching the exemplar's build_portal_nodes layout —
        // minimize (collapse control), header drag bar (pointer-only), composer
        // (reply text entry), submit (reply control), corner resize (pointer-only).
        let child = |scene: &mut SceneGraph, iid: &str, accepts_focus: bool| -> SceneId {
            let id = SceneId::new();
            scene.nodes.insert(
                id,
                Node {
                    layout: Default::default(),
                    id,
                    children: vec![],
                    data: NodeData::HitRegion(HitRegionNode {
                        bounds: Rect::new(0.0, 0.0, 40.0, 40.0),
                        interaction_id: iid.into(),
                        accepts_focus,
                        accepts_pointer: true,
                        ..Default::default()
                    }),
                },
            );
            id
        };
        let minimize = child(&mut scene, "portal-minimize", true);
        let drag = child(&mut scene, "portal-drag-header", false);
        let composer = child(&mut scene, "portal-composer-focus", true);
        let submit = child(&mut scene, "portal-submit", true);
        let resize = child(&mut scene, "portal-resize-bottom-right", false);

        let root = SceneId::new();
        scene.nodes.insert(
            root,
            Node {
                layout: Default::default(),
                id: root,
                children: vec![minimize, drag, composer, submit, resize],
                data: NodeData::SolidColor(tze_hud_scene::SolidColorNode {
                    color: tze_hud_scene::Rgba::new(0.0, 0.0, 0.0, 1.0),
                    bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
                    radius: None,
                }),
            },
        );
        scene.tiles.get_mut(&tile).unwrap().root_node = Some(root);

        let node = |id: SceneId| FocusOwner::Node {
            tile_id: tile,
            node_id: id,
        };

        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Tab forward: only the interactive controls, in DFS render order.
        // The pointer-only drag bar and resize handle are never visited.
        for expected in [minimize, composer, submit, minimize] {
            fm.navigate_next(tab_id, &scene);
            assert_eq!(
                fm.trees[&tab_id].current().clone(),
                node(expected),
                "Tab must stop only on interactive controls in render order"
            );
        }

        // Shift+Tab reverses the same ring (currently on `minimize`).
        for expected in [submit, composer, minimize] {
            fm.navigate_prev(tab_id, &scene);
            assert_eq!(
                fm.trees[&tab_id].current().clone(),
                node(expected),
                "Shift+Tab must reverse the ring"
            );
        }

        // The pointer-only handles never appear as focus owners.
        let cycle = build_focus_cycle(tab_id, &scene);
        for step in &cycle {
            if let CycleStep::Node { node_id, .. } = step {
                assert_ne!(*node_id, drag, "drag bar must not be a Tab stop");
                assert_ne!(*node_id, resize, "resize handle must not be a Tab stop");
            }
        }
        assert_eq!(
            cycle.len(),
            3,
            "exactly the three interactive controls form the ring"
        );
    }

    #[test]
    fn test_passthrough_tile_excluded_from_cycle() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();

        let lease_a = scene.grant_lease("a", 60_000);
        let lease_b = scene.grant_lease("b", 60_000);

        let t1 = scene
            .create_tile(tab_id, "a", lease_a, Rect::new(0.0, 0.0, 100.0, 100.0), 1)
            .unwrap();
        let t2_pt = scene
            .create_tile(tab_id, "b", lease_b, Rect::new(0.0, 0.0, 100.0, 100.0), 2)
            .unwrap();

        // t2 is passthrough.
        scene.tiles.get_mut(&t2_pt).unwrap().input_mode = InputMode::Passthrough;

        // t1: N1.
        let n1 = add_hit_region_direct(&mut scene, t1, "n1", true);

        // t2: has a focusable node but it's passthrough, so skipped.
        let _n_pt = add_hit_region_direct(&mut scene, t2_pt, "n_pt", true);

        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Cycle from None — should land on n1 (only non-passthrough step).
        fm.navigate_next(tab_id, &scene);
        let owner = fm.trees[&tab_id].current().clone();
        assert_eq!(
            owner,
            FocusOwner::Node {
                tile_id: t1,
                node_id: n1
            }
        );

        // Cycle again — no other non-passthrough steps, wraps to n1 again.
        fm.navigate_next(tab_id, &scene);
        let owner2 = fm.trees[&tab_id].current().clone();
        assert_eq!(
            owner2,
            FocusOwner::Node {
                tile_id: t1,
                node_id: n1
            }
        );
    }

    fn add_hit_region_direct(
        scene: &mut SceneGraph,
        tile_id: SceneId,
        interaction_id: &str,
        accepts_focus: bool,
    ) -> SceneId {
        let id = SceneId::new();
        scene.nodes.insert(
            id,
            Node {
                layout: Default::default(),
                id,
                children: vec![],
                data: NodeData::HitRegion(HitRegionNode {
                    bounds: Rect::new(0.0, 0.0, 50.0, 50.0),
                    interaction_id: interaction_id.into(),
                    accepts_focus,
                    accepts_pointer: true,
                    ..Default::default()
                }),
            },
        );
        scene.tiles.get_mut(&tile_id).unwrap().root_node = Some(id);
        id
    }

    // ── Focus events dispatch ────────────────────────────────────────────

    #[test]
    fn test_focus_events_dispatched_on_tab_key() {
        let (mut scene, tab_id, tile_id) = setup_scene();
        let _n1 = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "n1",
            true,
        );

        let lease_b = scene.grant_lease("b", 60_000);
        let t2 = scene
            .create_tile(tab_id, "b", lease_b, Rect::new(200.0, 0.0, 100.0, 100.0), 2)
            .unwrap();
        let n2 = add_hit_region(&mut scene, t2, Rect::new(0.0, 0.0, 50.0, 50.0), "n2", true);

        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Focus on t2/n2.
        fm.on_click(tab_id, t2, Some(n2), &scene);

        // Navigate prev → back to t1/n1.
        let t = fm.navigate_prev(tab_id, &scene);
        // Should have lost from b, gained to a.
        let (lost_ev, lost_ns) = t.lost.unwrap();
        let (gained_ev, gained_ns) = t.gained.unwrap();
        assert_eq!(lost_ev.reason, FocusLostReason::TabKey);
        assert_eq!(lost_ns, "b");
        assert_eq!(gained_ev.source, FocusSource::TabKey);
        // "agent-a" is the namespace set up by setup_scene().
        assert_eq!(gained_ns, "agent-a");
    }

    // ── Focus ring ───────────────────────────────────────────────────────

    #[test]
    fn test_focus_ring_bounds_computed_in_display_space() {
        let (mut scene, tab_id, tile_id) = setup_scene();
        // Tile at (100,100), node at local (10,20,50,30).
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(10.0, 20.0, 50.0, 30.0),
            "btn",
            true,
        );
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        let t = fm.on_click(tab_id, tile_id, Some(node_id), &scene);
        let ring = t.ring_update.unwrap();
        let b = ring.bounds.unwrap();
        // display_x = tile.x + node.x = 100 + 10 = 110
        assert!((b.x - 110.0).abs() < 0.001);
        assert!((b.y - 120.0).abs() < 0.001);
        assert!((b.width - 50.0).abs() < 0.001);
        assert!((b.height - 30.0).abs() < 0.001);
    }

    // ── Tab switch (spec lines 21-22) ────────────────────────────────────

    #[test]
    fn test_tab_switch_preserves_focus_without_events() {
        let (mut scene, tab_id, tile_id) = setup_scene();
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn",
            true,
        );
        let tab_b = scene.create_tab("B", 1).unwrap();

        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);
        fm.add_tab(tab_b);

        // Focus tab_a.
        fm.on_click(tab_id, tile_id, Some(node_id), &scene);
        let state_a_before = fm.trees[&tab_id].current().clone();

        // Switch to tab_b — no events should be emitted for tab_a.
        // (on_tab_switch only returns ring update, not FocusLostEvent)
        let _ring = fm.on_tab_switch(tab_id, tab_b, &scene);

        // tab_a focus unchanged.
        let state_a_after = fm.trees[&tab_id].current().clone();
        assert_eq!(state_a_before, state_a_after);
    }

    // ── Property test: focus uniqueness invariant ────────────────────────

    #[test]
    fn test_focus_uniqueness_under_transitions() {
        use std::collections::HashSet;

        let (mut scene, tab_id, tile_id) = setup_scene();
        let n1 = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 50.0, 50.0),
            "n1",
            true,
        );

        let lease_b = scene.grant_lease("b", 60_000);
        let t2 = scene
            .create_tile(tab_id, "b", lease_b, Rect::new(200.0, 0.0, 100.0, 100.0), 2)
            .unwrap();
        let n2 = add_hit_region(&mut scene, t2, Rect::new(0.0, 0.0, 50.0, 50.0), "n2", true);

        let lease_c = scene.grant_lease("c", 60_000);
        let t3 = scene
            .create_tile(tab_id, "c", lease_c, Rect::new(400.0, 0.0, 100.0, 100.0), 3)
            .unwrap();
        let n3 = add_hit_region(&mut scene, t3, Rect::new(0.0, 0.0, 50.0, 50.0), "n3", true);

        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);

        // Apply 30 random-ish transitions.
        let targets: Vec<(SceneId, SceneId)> = vec![(tile_id, n1), (t2, n2), (t3, n3)];
        for i in 0..30 {
            let (tile, node) = targets[i % targets.len()];
            fm.on_click(tab_id, tile, Some(node), &scene);

            // Invariant: exactly one current focus owner per tab.
            let owners: HashSet<String> = fm
                .trees
                .values()
                .map(|tree| format!("{:?}", tree.current()))
                .collect();
            // We only have one tab, so there's exactly one tree.
            assert_eq!(owners.len(), 1);
        }
    }

    // ── current_owner convenience accessor ───────────────────────────────────

    /// `current_owner` returns `FocusOwner::None` for an unregistered tab.
    #[test]
    fn current_owner_returns_none_for_unknown_tab() {
        let fm = FocusManager::new();
        let unknown_tab = SceneId::new();
        assert_eq!(
            fm.current_owner(unknown_tab),
            &FocusOwner::None,
            "unknown tab must produce FocusOwner::None"
        );
    }

    /// `current_owner` returns `FocusOwner::None` for a freshly registered tab
    /// (no click yet).
    #[test]
    fn current_owner_returns_none_before_first_click() {
        let mut fm = FocusManager::new();
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        fm.add_tab(tab_id);

        assert_eq!(
            fm.current_owner(tab_id),
            &FocusOwner::None,
            "no click yet — owner must be None"
        );
    }

    /// After a click on a focusable node, `current_owner` reflects the node.
    #[test]
    fn current_owner_reflects_click_to_focus_node() {
        let mut fm = FocusManager::new();
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        fm.add_tab(tab_id);

        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 200.0, 200.0),
                1,
            )
            .unwrap();
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn",
            true,
        );

        fm.on_click(tab_id, tile_id, Some(node_id), &scene);

        assert_eq!(
            fm.current_owner(tab_id),
            &FocusOwner::Node { tile_id, node_id },
            "current_owner must reflect the focused node after click"
        );
    }

    /// `current_owner` updates to `FocusOwner::None` after `remove_tab`.
    #[test]
    fn current_owner_none_after_tab_removed() {
        let mut fm = FocusManager::new();
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        fm.add_tab(tab_id);

        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(0.0, 0.0, 200.0, 200.0),
                1,
            )
            .unwrap();
        let node_id = add_hit_region(
            &mut scene,
            tile_id,
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "btn",
            true,
        );
        fm.on_click(tab_id, tile_id, Some(node_id), &scene);

        fm.remove_tab(tab_id);

        // After removal, the tab_id is no longer in the tree map.
        assert_eq!(
            fm.current_owner(tab_id),
            &FocusOwner::None,
            "removed tab must appear as FocusOwner::None"
        );
    }
}
