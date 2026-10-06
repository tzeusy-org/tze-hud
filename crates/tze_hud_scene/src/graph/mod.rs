//! Scene graph: the core data structure holding all tabs, tiles, nodes, leases.
//! Pure data — no GPU, no async, no I/O.

use crate::clock::{Clock, SystemClock};
use crate::types::*;
use crate::validation::ValidationError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Returns a `SystemClock` wrapped in `Arc<dyn Clock>`.
/// Used as the serde default for the `clock` field so that deserialized
/// graphs behave like freshly constructed ones.
fn default_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock::new())
}

/// Process-wide counter handing out unique [`SceneGraph::instance_id`] values
/// (hud-u4lq2).
static NEXT_SCENE_INSTANCE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Allocate a fresh, process-unique scene instance id.
///
/// Used both by [`SceneGraph::new_with_clock`] and as the serde default for
/// [`SceneGraph::instance_id`] (a deserialized/replayed graph is a DIFFERENT
/// live instance from whatever produced the snapshot, so it gets its own id
/// rather than carrying one over the wire).
fn next_scene_instance_id() -> u64 {
    NEXT_SCENE_INSTANCE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The root scene graph.
///
/// Time-dependent operations (lease grant, tab creation timestamps, expiry
/// checks) are routed through the injected [`Clock`].  Use
/// [`SceneGraph::new`] for production code — it installs a [`SystemClock`].
/// Use [`SceneGraph::new_with_clock`] in tests to inject a [`TestClock`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SceneGraph {
    /// Clock used for all `now_millis()` calls inside the graph.
    /// Skipped during serialization; restored to `SystemClock` on
    /// deserialization.
    #[serde(skip, default = "default_clock")]
    clock: Arc<dyn Clock>,
    /// Process-unique identity of THIS in-memory `SceneGraph` object, distinct
    /// from [`Self::version`] (hud-u4lq2).
    ///
    /// `version` is a per-instance mutation counter that restarts at 0 for
    /// every new `SceneGraph` — it is NOT a reliable "have I already seen this
    /// exact scene" signal across different instances, because two unrelated
    /// `SceneGraph`s can easily reach the identical `version` number (e.g. the
    /// same setup sequence run twice). A long-lived `Compositor` that gets
    /// handed a DIFFERENT `SceneGraph` (a fresh instance, not just a mutation of
    /// the one it already saw) needs a signal that changes on EVERY new
    /// instance regardless of its `version` value, so a stale "already primed"
    /// guard (e.g. `Compositor::markdown_cache_scene_version`) cannot silently
    /// skip priming for the new instance forever. Skipped during serialization
    /// (a deserialized/replayed graph is a distinct live instance, not a
    /// continuation of whatever produced the snapshot) — restored via a fresh
    /// process-unique id on deserialize, same as a freshly constructed graph.
    #[serde(skip, default = "next_scene_instance_id")]
    pub instance_id: u64,
    /// All tabs, keyed by ID.
    pub tabs: HashMap<SceneId, Tab>,
    /// The currently active tab.
    pub active_tab: Option<SceneId>,
    /// All tiles, keyed by ID.
    pub tiles: HashMap<SceneId, Tile>,
    /// All nodes, keyed by ID.
    pub nodes: HashMap<SceneId, Node>,
    /// Active leases, keyed by ID.
    pub leases: HashMap<SceneId, Lease>,
    /// Hit region local state, keyed by node ID.
    pub hit_region_states: HashMap<SceneId, HitRegionLocalState>,
    /// Zone registry.
    pub zone_registry: ZoneRegistry,
    /// Widget registry.
    pub widget_registry: WidgetRegistry,
    /// Display area (the viewport dimensions).
    pub display_area: Rect,
    /// Monotonic version counter, incremented on every mutation.
    pub version: u64,
    /// Monotonic epoch for **position-only** geometry mutations (portal/tile
    /// drag-move).
    ///
    /// A pure translation moves surfaces without changing their size or content,
    /// so it must NOT invalidate the compositor's content-shaped caches
    /// (markdown parse, ellipsis truncation) — those are gated on
    /// [`Self::version`] and keyed on content + size, never on x/y position.
    /// Bumping `version` on every drag-move pointer delta forced a full
    /// re-hash / re-shape per frame, which showed up live as low-fps, flickery
    /// drag (hud-uyhpn).
    ///
    /// Instead the drag-move path calls [`Self::bump_geometry_epoch`], which
    /// advances this counter (re-arming the idle present-gate so the new
    /// position paints every frame) while leaving `version` — and therefore the
    /// content caches — untouched. The render dirty-gate treats a change in
    /// EITHER `version` or `geometry_epoch` as "needs a frame".
    ///
    /// Position-only: `version` still increments on size/content/structural
    /// mutations (resize, add/remove, content edit), so those correctly
    /// re-prime the caches.
    #[serde(default)]
    pub geometry_epoch: u64,
    /// Monotonically increasing sequence number assigned to each committed batch.
    ///
    /// Incremented by [`SceneGraph::next_sequence_number`] on every successful
    /// [`crate::mutation::MutationBatch`] commit. Per RFC 0001 §3.5.
    pub sequence_number: u64,
    /// Batches held until their `present_at` time (see [`timed`]).
    #[serde(skip, default)]
    pub scheduled_batches: Vec<timed::ScheduledBatch>,
    /// Notification action presses waiting for their publisher to collect
    /// them (MCP `hud_input`). Bounded; oldest dropped first.
    #[serde(skip, default)]
    pub pending_actions: std::collections::VecDeque<PendingAction>,
    /// Map of ResourceIds to their scene-node reference counts.
    ///
    /// A resource is available for use in [`NodeData::StaticImage`] nodes when it
    /// has an entry in this map (regardless of the count value).  The count tracks
    /// how many live scene nodes currently reference the resource:
    ///
    /// - `register_resource` inserts the entry (count = 0) if not already present.
    /// - Inserting a `StaticImageNode` into the scene increments the count.
    /// - Removing a `StaticImageNode` from the scene decrements the count; when the
    ///   count reaches zero the entry is removed, freeing the resource from the
    ///   registry.
    ///
    /// An agent-submitted AddNode or SetTileRoot with a StaticImageNode is rejected
    /// if the ResourceId is not present in this map.
    ///
    /// Ephemeral: skipped during serialization (resources are in-memory only,
    /// per RFC 0001 §2.4 and resource-store/spec.md §Requirement: V1 ephemerality).
    #[serde(skip, default)]
    pub registered_resources: HashMap<ResourceId, u32>,

    /// Transient per-frame state owned by the runtime and compositor layers.
    ///
    /// Groups all render-scratch and input-feedback fields that must not pollute
    /// the pure scene-model public surface.  See [`RuntimeOverlayState`] for the
    /// full field inventory and lifetime semantics.
    #[serde(skip, default)]
    pub overlay: RuntimeOverlayState,
}

/// Maximum number of tabs in a scene. RFC 0001 §2.1.
pub const MAX_TABS: usize = 256;

/// Maximum number of tiles per tab. RFC 0001 §2.1.
pub const MAX_TILES_PER_TAB: usize = 1024;

/// Maximum number of nodes per tile. RFC 0001 §2.1.
pub const MAX_NODES_PER_TILE: usize = 64;

/// Maximum name length for tabs, in UTF-8 bytes. RFC 0001 §2.2.
pub const MAX_TAB_NAME_BYTES: usize = 128;

/// Maximum content size for TextMarkdownNode, in UTF-8 bytes. RFC 0001 §2.4.
pub const MAX_MARKDOWN_BYTES: usize = 65_535;

/// The z-order threshold below which agent-owned tiles must fall.
/// Tiles with z_order >= ZONE_TILE_Z_MIN are reserved for runtime-managed
/// zone tiles. RFC 0001 §2.3.
pub const ZONE_TILE_Z_MIN: u32 = 0x8000_0000;

// ─── Contention policy helper ────────────────────────────────────────────────

pub mod contention;
pub(crate) use contention::apply_contention;

pub mod overlay;
pub use overlay::RuntimeOverlayState;

pub mod budget;
pub mod leases;
pub mod node_tree;
pub mod queries;
pub mod resources;
pub mod snapshot;
pub mod tabs;
pub mod tiles;
pub mod timed;
pub use tiles::validate_text_markdown_node_data;
pub mod zone_ops;

/// A notification action the human pressed, addressed to the agent that
/// published the notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingAction {
    pub publisher_namespace: String,
    pub zone_name: String,
    pub callback_id: String,
}

/// Cap on queued action presses; a publisher that never collects them can't
/// grow the queue without bound.
pub const MAX_PENDING_ACTIONS: usize = 256;

impl SceneGraph {
    // ─── Notification auto-dismiss TTL constants ─────────────────────────
    /// Default auto-dismiss TTL (µs) for low/normal notifications (urgency 0, 1).
    pub const NOTIFICATION_TTL_INFO_US: u64 = 8_000_000; // 8 seconds
    /// Default auto-dismiss TTL (µs) for urgent notifications (urgency 2).
    pub const NOTIFICATION_TTL_WARNING_US: u64 = 15_000_000; // 15 seconds
    /// Default auto-dismiss TTL (µs) for critical notifications (urgency 3+).
    pub const NOTIFICATION_TTL_CRITICAL_US: u64 = 30_000_000; // 30 seconds

    /// Create a new empty scene graph using the real system clock.
    pub fn new(width: f32, height: f32) -> Self {
        Self::new_with_clock(width, height, Arc::new(SystemClock::new()))
    }

    /// Create a new empty scene graph with an injected clock.
    ///
    /// Prefer this constructor in tests so that time-dependent behaviour
    /// (lease expiry, timestamps) is fully deterministic.
    pub fn new_with_clock(width: f32, height: f32, clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            instance_id: next_scene_instance_id(),
            tabs: HashMap::new(),
            active_tab: None,
            tiles: HashMap::new(),
            nodes: HashMap::new(),
            leases: HashMap::new(),
            hit_region_states: HashMap::new(),
            zone_registry: ZoneRegistry::new(),
            widget_registry: WidgetRegistry::new(),
            display_area: Rect::new(0.0, 0.0, width, height),
            version: 0,
            geometry_epoch: 0,
            sequence_number: 0,
            scheduled_batches: Vec::new(),
            pending_actions: std::collections::VecDeque::new(),
            registered_resources: HashMap::new(),
            overlay: RuntimeOverlayState::default(),
        }
    }

    /// Queue a notification action press for its publisher.
    pub fn push_pending_action(&mut self, action: PendingAction) {
        if self.pending_actions.len() >= MAX_PENDING_ACTIONS {
            self.pending_actions.pop_front();
        }
        self.pending_actions.push_back(action);
    }

    /// Remove and return the queued action presses for `namespace`, oldest first.
    pub fn take_pending_actions(&mut self, namespace: &str) -> Vec<PendingAction> {
        let (mine, rest): (Vec<_>, Vec<_>) = self
            .pending_actions
            .drain(..)
            .partition(|a| a.publisher_namespace == namespace);
        self.pending_actions = rest.into();
        mine
    }

    // ─── Sequence number (RFC 0001 §3.5) ────────────────────────────────

    /// Advance the sequence counter and return the new value.
    ///
    /// Called by the mutation pipeline on every successful batch commit.
    /// Sequence numbers are strictly monotonically increasing u64 values.
    pub(crate) fn next_sequence_number(&mut self) -> u64 {
        self.sequence_number += 1;
        self.sequence_number
    }

    // ─── Position-only geometry epoch (hud-uyhpn) ───────────────────────

    /// Signal a **position-only** geometry mutation (drag-move translation).
    ///
    /// Advances [`Self::geometry_epoch`] so the runtime's idle present-gate
    /// repaints the moved surfaces on the next frame, WITHOUT bumping
    /// [`Self::version`]. Because the compositor's markdown-parse and
    /// ellipsis-truncation caches are gated on `version` (content + size, never
    /// x/y), skipping the version bump means a smooth drag never triggers a
    /// per-frame re-hash / re-shape — the low-fps drag fix for hud-uyhpn.
    ///
    /// Use this ONLY for pure translations (bounds x/y change, width/height and
    /// content unchanged). Any mutation that changes size, content, or scene
    /// structure MUST bump `version` so the caches re-prime.
    #[inline]
    pub fn bump_geometry_epoch(&mut self) {
        self.geometry_epoch += 1;
    }

    // ─── Clock accessor ──────────────────────────────────────────────────

    /// Return the current time in milliseconds from the injected clock.
    ///
    /// Public so callers that stamp lease lifecycle transitions
    /// (`disconnect_lease`/`reconnect_lease`) use the SAME clock domain the
    /// lease expiry/grace checks compare against — a
    /// wall-clock value from a different source would make grace bounds
    /// inconsistent.
    pub fn now_millis(&self) -> u64 {
        self.clock.now_millis()
    }

    /// Current wall-clock time in microseconds from the injected clock, the
    /// domain `present_at` / `expires_at` deadlines are compared in.
    pub fn now_wall_us(&self) -> u64 {
        self.clock.now_us()
    }

    /// Monotonic microseconds from the injected clock, for measuring elapsed
    /// time (animations); never steps backward on a real clock.
    pub fn now_monotonic_us(&self) -> u64 {
        self.clock.monotonic_us()
    }
}

#[cfg(test)]
mod tests;

// Behavior suites share fixtures only in lib-test builds.
#[cfg(test)]
mod limits_tests;
#[cfg(test)]
mod spec_scenarios;
#[cfg(test)]
mod test_helpers;
#[cfg(test)]
mod widget_tests;
