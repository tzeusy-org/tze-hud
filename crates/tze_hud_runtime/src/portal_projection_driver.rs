//! In-process portal projection authority driver (hud-2iup7).
//!
//! This module hosts a [`ProjectionAuthority`] directly inside the runtime
//! process and drives the portal drain loop from the winit event-loop
//! `about_to_wait` callback (composer-flush pattern).
//!
//! ## Architecture decision (hud-6k06b)
//!
//! The projection authority is hosted in-process rather than as a stdio
//! subprocess. The stdio binary (`projection_authority`) and its tests remain
//! as the component harness; this driver is the production integration path.
//!
//! ## Drain loop (spec §3.2 / §3.3)
//!
//! On every `about_to_wait` the driver:
//! 1. Acquires `ProjectionAuthority::next_due_projection_id()` in round-robin order.
//! 2. Materialises the coalesced transcript window via `take_due_portal_update`.
//! 3. Builds the portal state via `projected_portal_state`.
//! 4. For `CreatePortalTile` records: creates a new tile in the scene graph,
//!    registers its scroll config, and records the tile ID in the adapter.
//! 5. For `RenderPortal` records: calls
//!    [`InputProcessor::notify_tile_content_appended`] with the append geometry
//!    so follow-tail advances (spec §3.2) or remains stable when scrolled-back
//!    (spec §3.3 — the `InputProcessor` enforces this; the driver just calls it).
//!
//! ## Lock discipline
//!
//! The `drain` method takes a direct `&mut SceneGraph` and `&mut InputProcessor`
//! reference (not locks), so it is synchronous and lock-free.  The caller
//! (windowed runtime) is responsible for acquiring the scene lock with
//! `try_lock` and deferring the call to the next `about_to_wait` if busy.
//!
//! ## Panic safety
//!
//! The `drain` method is wrapped in `std::panic::catch_unwind`. A panic resets
//! the driver's drive state and logs an error; it does NOT propagate to the
//! event loop.
//!
//! ## Hook points
//!
//! - **hud-ttq97** (submitted_at_us telemetry bucket): `submitted_at_us` from
//!   `PortalTranscriptUpdate` is consumed by the drain loop into
//!   [`InProcessPortalDriver::portal_publish_to_present_latency`], measuring
//!   the end-to-end publish→present latency for each coalesced portal update.
//! - **hud-pkg2g** (head-trim notify_head_content_removed): wired — the drain
//!   loop detects head-trim via `visible_transcript_bytes` / content-height
//!   decrease and calls `notify_head_content_removed` on the `InputProcessor`
//!   so scrolled-back viewports stay stable (spec §3.3).

use std::collections::HashMap;

use tze_hud_config::{resolve_portal_tokens, tokens::DesignTokenMap};
use tze_hud_input::{DraftNotificationBatch, InputProcessor};
pub use tze_hud_mcp::portal_op::{
    PendingInputBatch, PendingInputEntry, PortalOp, PortalOpRejection, ProjectionListBatch,
    ProjectionListEntry,
};
use tze_hud_projection::{
    AcknowledgeInputRequest, AdapterDraftBatch, AdapterDraftCancel, AdapterDraftNotification,
    AdapterDraftSubmission, AdapterGeometrySnapshot, AdapterPortalRect, AttachRequest,
    CleanupAuthority, CleanupRequest, ContentClassification, DetachRequest, GetPendingInputRequest,
    HudConnectionMetadata, InputAckState, ListProjectionsRequest, OperationEnvelope, OutputKind,
    PendingInputItem, PortalInputFeedback, ProjectedPortalPolicy, ProjectionAuthority,
    ProjectionBounds, ProjectionErrorCode, ProjectionLifecycleState, ProjectionOperation,
    ProviderKind, PublishOutputRequest, PublishStatusRequest,
    resident_grpc::{
        ResidentGrpcPortalAdapter, ResidentGrpcPortalCommandKind, ResidentGrpcPortalConfig,
        portal_visual_tokens_from_part_tokens,
    },
};
use tze_hud_scene::{
    Rect, SceneGraph,
    types::{LeaseState, SceneId, TileScrollConfig},
};
use tze_hud_telemetry::LatencyBucket;

use crate::idle_efficiency::RuntimeWakeupSource;
use crate::resident_grpc_bridge::BridgeMessage;

/// Which transport materialises a portal projection's scene presence (hud-g7ool).
///
/// v1 routing policy (owner decision 2026-07-04, OPTION B): each projection is
/// materialised by EXACTLY ONE transport — bridge XOR in-process.
///
/// - [`PortalTransport::InProcess`] (the default) paints the projection's tile
///   directly on the winit thread via the in-process direct-scene path.
/// - [`PortalTransport::ResidentGrpcBridge`] routes the projection's coalesced
///   state to the resident gRPC bridge, which materialises it over an
///   authenticated `HudSession` stream. When a projection is bridged, its
///   in-process direct-scene materialisation is SUPPRESSED so the two transports
///   never double-paint one scene (the original hud-d7frs double-materialisation
///   bug).
///
/// This discriminant is the foundation the completeness cluster (hud-omfqi,
/// hud-ygtiy) builds on. A routed-to-bridge projection whose bridge channel is
/// not wired (or has closed) falls back to the in-process path — see
/// [`InProcessPortalDriver::effective_transport`] — so a projection routed to a
/// dead bridge still materialises somewhere rather than vanishing (fail-safe).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PortalTransport {
    /// In-process direct-scene materialisation (default).
    #[default]
    InProcess,
    /// The resident gRPC bridge is the sole materialiser for this projection.
    ResidentGrpcBridge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum PortalDeadlineFamily {
    ImmediateWork,
    Cadence,
    ActivityCue,
    AgentLiveness,
    PendingInputExpiry,
    LeaseLifecycle,
}

impl PortalDeadlineFamily {
    pub(super) const fn wakeup_source(self) -> RuntimeWakeupSource {
        match self {
            Self::ImmediateWork => RuntimeWakeupSource::SceneChange,
            Self::Cadence | Self::ActivityCue => RuntimeWakeupSource::AnimationDeadline,
            Self::AgentLiveness | Self::PendingInputExpiry | Self::LeaseLifecycle => {
                RuntimeWakeupSource::TtlDeadline
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PortalWakeDeadline {
    pub(super) wall_us: u64,
    pub(super) family: PortalDeadlineFamily,
}

/// Line-height multiplier used by the compositor's text shaper (text.rs).
///
/// `line_height_px = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER`
///
/// Must stay in sync with `tze_hud_compositor::text` — search for `1.4` there.
const PORTAL_LINE_HEIGHT_MULTIPLIER: f32 = 1.4;

/// Namespace used by the in-process projection driver for scene mutations.
///
/// This namespace is used when creating portal tiles and publishing content.
/// It is distinct from any agent-facing namespace.
pub const PORTAL_DRIVER_NAMESPACE: &str = "tze_hud_portal_driver";

/// Projection caller identity at the authority seam.
///
/// MCP passes one stable non-secret identity for every caller today;
/// per-agent ownership is enforced in the MCP server, which keys each
/// projection's owner token by (agent id, projection id). The
/// authority still enforces its caller-scoped filter, allowing a future
/// authenticated multi-principal transport to pass distinct identities without
/// broadening this list operation's visibility.
const MCP_PORTAL_CALLER_IDENTITY: &str = "mcp-portal";

/// Default z-order for portal tiles created by the in-process driver.
const PORTAL_Z_ORDER: u32 = 160;

/// Hard upper bound on the number of portal updates processed in a single
/// `drain_inner` call (per `about_to_wait` tick).
///
/// The drain runs on the winit event-loop thread while holding the scene lock,
/// so an unbounded inner loop wedges the entire
/// presentation pipeline (whole-HUD freeze, one core pegged ~98%). The loop is
/// normally work-conserving and terminates when the coalescer is drained, but a
/// divergence between the session map and the coalescer (e.g. an orphaned
/// coalescer entry whose session was removed by operator cleanup — hud-bsr7u)
/// could make `next_due_projection_id` return the same id forever.
///
/// This cap is a defense-in-depth backstop: even if a future divergence
/// re-introduces such an entry, the loop can never spin the event loop more than
/// this many iterations per tick. The value is far above the realistic number of
/// concurrent portals (≤ 8 in practice, each contributing at most one update per
/// drain), so it never truncates legitimate work; reaching it indicates a bug
/// and is logged at `error` level.
const MAX_PORTAL_DRAIN_ITERATIONS_PER_CYCLE: u32 = 1024;

/// Parse an optional snake_case `output_kind` string into [`OutputKind`].
///
/// `None` defaults to [`OutputKind::Assistant`] (the contract default). The
/// value is deserialized through the same serde `snake_case` representation
/// the wire contract uses, so accepted spellings match
/// `serde(rename_all = "snake_case")` exactly. An unrecognized value yields an
/// `Err` describing the rejection, which the caller forwards to the requester
/// rather than silently coercing to a default.
///
/// `"viewer"` is explicitly reserved: it is appended internally by
/// `submit_portal_input` to echo viewer-submitted text into the portal
/// transcript and MUST NOT be publishable by an external agent.
fn parse_output_kind(raw: Option<&str>) -> Result<OutputKind, String> {
    match raw {
        None => Ok(OutputKind::default()),
        Some("viewer") => Err(
            "output_kind \"viewer\" is reserved for viewer-submitted input \
             and cannot be published by an agent"
                .to_string(),
        ),
        Some(value) => serde_json::from_value(serde_json::Value::String(value.to_string()))
            .map_err(|_| {
                format!(
                    "invalid output_kind {value:?}: expected one of \
                     assistant, tool, status, error, other"
                )
            }),
    }
}

/// Refresh a render state's unread-output count field from the freshly
/// drained batch, preserving redaction (hud-meqet; extended to the
/// clearance-corrected divider count by hud-n95zc).
///
/// `take_due_portal_update` zeroes `session.unread_output_count` as it consumes
/// a drain, so the subsequent `projected_portal_state` reads a stale `0` for
/// EVERY field it derives from that session field — both the ambient
/// `unread_output_count` ("N unread" ticker) and `visible_unread_output_count`
/// (the in-transcript divider boundary, `resident_grpc::unread_divider_boundary`)
/// — the live count survives only on the drained `PortalTranscriptUpdate`. This
/// maps a render state's already-gated field to the drained count:
///
/// - `Some(_)` — the `reveal_unread` policy gate revealed the count, so refresh
///   it to `drained` (this is the fix: `Some(0)` from the zeroed session becomes
///   `Some(drained)`, un-suppressing the indicator/divider on a real coalesced
///   drain). Callers pass this the raw drained count for both fields: every
///   production call site uses `ProjectedPortalPolicy::permit_all()`, so no
///   viewer-clearance filtering ever removes a unit from `visible_transcript`
///   and the clearance-corrected count coincides with the aggregate.
/// - `None` — the count was redacted upstream; leave it redacted. We never
///   resurrect a redacted `None` into a leaked count.
///
/// A `drained` value of `0` maps a revealed slot to `Some(0)`, which the
/// downstream indicator/divider suppresses exactly like the redacted `None`.
fn carry_drained_unread_count(state_count: Option<usize>, drained: usize) -> Option<usize> {
    state_count.map(|_| drained)
}

/// The wall-clock instant (µs) at which the drive loop should force one repaint
/// so `state`'s agent-activity / streaming-cursor cue quiesces (hud-kbm80), or
/// `None` when the state carries no cue active as of `now_us` — in which case
/// there is nothing to schedule a quiesce for.
///
/// Returns the first instant the cue reads false (the derivation deadline from
/// [`resident_grpc::agent_activity_clear_deadline_us`] plus one µs), but only
/// when that deadline is still ahead of `now_us` (the cue is live now). A tail
/// already stale at render time needs no scheduled repaint — it is quiesced the
/// moment it paints — so this returns `None` for it, clearing any prior deadline.
fn activity_cue_clear_due_us(
    state: &tze_hud_projection::ProjectedPortalState,
    now_us: u64,
) -> Option<u64> {
    tze_hud_projection::resident_grpc::agent_activity_clear_deadline_us(state)
        .filter(|deadline| *deadline >= now_us)
        .map(|deadline| deadline.saturating_add(1))
}

/// Parse an optional snake_case `content_classification` string into
/// [`ContentClassification`].
///
/// `None` defaults to [`ContentClassification::Private`] — privacy is
/// safe-by-default per the cooperative-hud-projection privacy governance
/// requirement. An unrecognized value yields an `Err` rather than coercing to a
/// (potentially less private) default.
fn parse_content_classification(raw: Option<&str>) -> Result<ContentClassification, String> {
    match raw {
        None => Ok(ContentClassification::default()),
        Some(value) => serde_json::from_value(serde_json::Value::String(value.to_string()))
            .map_err(|_| {
                format!(
                    "invalid content_classification {value:?}: expected one of \
                     public, household, private, sensitive"
                )
            }),
    }
}

/// Parse an optional snake_case `provider_kind` string into [`ProviderKind`].
///
/// `None` defaults to [`ProviderKind::Other`]. An unrecognized value yields an
/// `Err` describing the rejection, which the caller forwards to the requester
/// rather than silently coercing to a default.
fn parse_provider_kind(raw: Option<&str>) -> Result<ProviderKind, String> {
    match raw {
        None => Ok(ProviderKind::Other),
        Some(value) => serde_json::from_value(serde_json::Value::String(value.to_string()))
            .map_err(|_| {
                format!(
                    "invalid provider_kind {value:?}: expected one of \
                     codex, claude, opencode, other"
                )
            }),
    }
}

/// Parse a snake_case `ack_state` string into [`InputAckState`].
///
/// Accepts exactly the wire spellings of `serde(rename_all = "snake_case")`
/// (`handled`, `deferred`, `rejected`). Unlike the publish-output parsers there
/// is no default: acknowledgement state is mandatory, so an empty or
/// unrecognized value yields an `Err` describing the rejection, which the
/// caller forwards to the requester.
fn parse_ack_state(raw: &str) -> Result<InputAckState, String> {
    serde_json::from_value(serde_json::Value::String(raw.to_string())).map_err(|_| {
        format!("invalid ack_state {raw:?}: expected one of handled, deferred, rejected")
    })
}

/// Parse a snake_case `lifecycle_state` string into [`ProjectionLifecycleState`].
///
/// Accepts exactly the wire spellings of `serde(rename_all = "snake_case")`
/// (`attached`, `active`, `degraded`, `hud_unavailable`, `detached`,
/// `cleanup_pending`, `expired`). There is no default: the lifecycle state is
/// the whole point of `publish_status`, so an empty or unrecognized value yields
/// an `Err` that the caller forwards to the requester before the authority sees
/// it.
fn parse_lifecycle_state(raw: &str) -> Result<ProjectionLifecycleState, String> {
    serde_json::from_value(serde_json::Value::String(raw.to_string())).map_err(|_| {
        format!(
            "invalid lifecycle_state {raw:?}: expected one of attached, active, \
             degraded, hud_unavailable, detached, cleanup_pending, expired"
        )
    })
}

/// Serialize a [`ProjectionLifecycleState`] back to its snake_case wire string.
///
/// Used to echo the authority's applied lifecycle state back through the
/// `publish_status` reply channel so the round-trip is observable by the MCP
/// caller. The enum is a fixed `serde(rename_all = "snake_case")` set, so
/// serialization is infallible in practice; an unexpected failure falls back to
/// the internal-error code spelling rather than panicking.
fn lifecycle_state_wire(state: ProjectionLifecycleState) -> String {
    match serde_json::to_value(state) {
        Ok(serde_json::Value::String(s)) => s,
        _ => "active".to_string(),
    }
}

/// Parse a snake_case cleanup authority string into [`CleanupAuthority`].
///
/// Accepted spellings match the projection contract enum exactly (`owner`,
/// `operator`). Rejection happens before reaching the authority so malformed MCP
/// requests cannot be silently treated as owner or operator cleanup.
fn parse_cleanup_authority(raw: &str) -> Result<CleanupAuthority, String> {
    serde_json::from_value(serde_json::Value::String(raw.to_string()))
        .map_err(|_| format!("invalid cleanup_authority {raw:?}: expected owner or operator"))
}

fn adapter_draft_batch_from_runtime(batch: &DraftNotificationBatch) -> AdapterDraftBatch {
    AdapterDraftBatch {
        latest: batch
            .latest
            .as_ref()
            .map(|latest| AdapterDraftNotification {
                text: latest.text.clone(),
                cursor: latest.cursor,
                selection_anchor: latest.selection_anchor,
                at_capacity: latest.at_capacity,
                sequence: latest.sequence,
            }),
        submission: batch
            .submission
            .as_ref()
            .map(|submission| AdapterDraftSubmission {
                text: submission.text.clone(),
                sequence: submission.sequence,
            }),
        cancel: batch.cancel.as_ref().map(|cancel| AdapterDraftCancel {
            sequence: cancel.sequence,
        }),
    }
}

/// Map an authority [`PendingInputItem`] into the transport-layer
/// [`PendingInputEntry`] returned through the portal-op reply channel.
///
/// The enum fields (`delivery_state`, `content_classification`) are converted to
/// their snake_case wire spellings via serde so the MCP JSON-RPC response uses
/// the same vocabulary as the rest of the projection contract. The serde
/// round-trip cannot fail for these C-like enums; the `unwrap_or_else` fallback
/// is purely defensive and never taken in practice.
fn pending_input_entry_from_item(item: &PendingInputItem) -> PendingInputEntry {
    PendingInputEntry {
        input_id: item.input_id.clone(),
        projection_id: item.projection_id.clone(),
        submission_text: item.submission_text.clone(),
        submitted_at_wall_us: item.submitted_at_wall_us,
        expires_at_wall_us: item.expires_at_wall_us,
        delivery_state: serde_json::to_value(item.delivery_state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string()),
        content_classification: serde_json::to_value(item.content_classification)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string()),
    }
}

/// Per-projection adapter state managed by the in-process driver.
struct DriveEntry {
    /// gRPC adapter that renders markdown and tracks tile state.
    adapter: ResidentGrpcPortalAdapter,
    /// SceneGraph lease governing this projection's stale-surface grace.
    ///
    /// In-process projections attach their tile to this lease. Bridge-routed
    /// projections deliberately keep it tile-less: the SceneGraph lease state
    /// remains the single grace clock while the bridge owns the visible tile.
    scene_lease_id: Option<SceneId>,
    /// Scene tile ID assigned to this portal, or `None` if not yet created.
    tile_scene_id: Option<SceneId>,
    /// Estimated total content height (px) from the last `RenderPortal` drain.
    ///
    /// A decrease in this value between consecutive drains is the authoritative
    /// signal that content was removed from the head of the visible transcript
    /// (head-trim event).  `notify_head_content_removed` is called with the
    /// difference so a scrolled-back viewport stays stable (spec §3.3 /
    /// hud-pkg2g, hud-66i1s).
    ///
    /// This is the height-only condition, mirroring the fix in
    /// `projection_authority.rs` (hud-hkaw2 / PR #779).
    prev_content_height_px: f32,
    /// Whether the driver has latched this projection's upstream as ungracefully
    /// dropped (hud-5i16d).
    ///
    /// Set by [`InProcessPortalDriver::mark_projection_disconnected_at`] when an
    /// ungraceful upstream drop is detected (e.g. the MCP `portal_op` channel
    /// closes without a clean `Detach`/`Cleanup`).  While set, the authority's
    /// `connection_degraded` latch is true and the surface presents the degraded
    /// treatment.  Cleared by
    /// [`InProcessPortalDriver::clear_projection_disconnect_at`] on the next owner
    /// publish / re-attach (the reconnect signal), which calls
    /// `record_hud_connection`.
    ///
    /// This mirrors the authority's own latch — the driver is the sole caller of
    /// `mark_hud_disconnected`/`record_hud_connection` for the in-process path —
    /// so it tracks whether a reconnect must restore the connection rather than
    /// re-deriving that from authority state every publish (which would inflate
    /// the reconnect bookkeeping on normal, non-recovery publishes).
    hud_disconnected: bool,
    /// Agent-op staleness latch, distinct from HUD transport disconnection.
    /// Both use the same stale treatment and scene lease grace path.
    agent_liveness_degraded: bool,
    /// One-shot flag requesting a forced degraded repaint on the next drain
    /// (hud-h3mvo).
    ///
    /// A pure upstream drop (no subsequent publish) latches `hud_disconnected`
    /// and flips the authority's `connection_degraded`, but adds no coalescer
    /// update — so the round-robin due-loop in [`InProcessPortalDriver::drain_inner`]
    /// never revisits this projection and the scene tile keeps its un-dimmed
    /// paint until the *next* publish happens to arrive. Set here on the
    /// disconnect transition so a post-due-loop pass repaints the tile once
    /// (dim + any degraded affordance) within one frame; cleared after that
    /// repaint, on any normal render, and on reconnect — so the pass is
    /// one-shot rather than re-rendering an idle degraded tile every drain.
    needs_degraded_repaint: bool,
    /// One-shot wall-clock deadline (µs since epoch) at which the drive loop must
    /// force a repaint so the agent-activity / streaming-cursor cue quiesces
    /// (hud-kbm80).
    ///
    /// The ambient "⋯ writing" header line + "▍" tail cursor are DERIVED from the
    /// newest transcript unit's `appended_at_wall_us` vs the render `now`
    /// (`resident_grpc::agent_activity_active`) and quiesce once that tail ages
    /// past `PORTAL_ACTIVITY_QUIESCE_WINDOW_US`. But the round-robin due-loop in
    /// [`InProcessPortalDriver::drain_inner`] only re-renders a portal on a fresh
    /// coalescer update — so after the terminal append on an otherwise-idle
    /// portal nothing re-evaluates the cue and it would persist indefinitely,
    /// misrepresenting ongoing activity. Each materialisation records the instant
    /// the cue first reads false (the derivation deadline + 1); a post-due-loop
    /// pass forces one repaint at/after it, then clears this back to `None`
    /// (one-shot). A fresh append re-materialises and overwrites the deadline, so
    /// continued streaming simply extends it; a state that carries no active cue
    /// clears it.
    activity_cue_clear_due_us: Option<u64>,
    /// The raw unread-output count carried from the most recent materialisation's
    /// drained batch (hud-kbm80 follow-up).
    ///
    /// `take_due_portal_update` zeroes `session.unread_output_count` as it consumes
    /// a drain, so the normal drain restores the live count from the drained
    /// `PortalTranscriptUpdate` via [`carry_drained_unread_count`]. The forced
    /// cue-quiesce repaint has no such drained update in hand — it re-derives state
    /// straight from the (already-zeroed) session — so without this it would repaint
    /// the ambient "N unread" indicator away even though no viewer action cleared it.
    /// We stash the last-materialised raw count here and re-apply it (through
    /// `carry_drained_unread_count`, preserving redaction) on the quiesce repaint.
    activity_cue_carried_unread: usize,
}

/// In-process state for the portal projection drive loop.
///
/// This is the runtime-side equivalent of `PortalDriveState` in the stdio
/// projection_authority binary. It holds one `ResidentGrpcPortalAdapter` per
/// attached projection session plus tile-to-scene mapping.
struct InProcessPortalDriveState {
    /// Per-projection drive entries keyed by `projection_id`.
    entries: HashMap<String, DriveEntry>,
    /// Scene tiles whose projection state has been accepted for detach/cleanup,
    /// but whose tile removal must wait until the next drain has scene access.
    pending_tile_removals: Vec<SceneId>,
    /// Projection leases whose clean detach was accepted while scene access was
    /// unavailable. Revoking them on the next drain prevents empty 24-hour lease
    /// records from accumulating after normal detach/reattach cycles.
    pending_lease_revocations: Vec<SceneId>,
    /// Current resolved design-token overrides (flat key → value strings).
    token_overrides: DesignTokenMap,
    /// Per-projection transport routing (hud-g7ool). Absent ⇒ the default
    /// [`PortalTransport::InProcess`]. Kept independent of `entries` (rather than
    /// on `DriveEntry`) so a projection's transport can be set without an attached
    /// in-process drive entry, and so the default in-process path is byte-for-byte
    /// unchanged when nothing is routed to the bridge.
    projection_transports: HashMap<String, PortalTransport>,
}

impl InProcessPortalDriveState {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            pending_tile_removals: Vec::new(),
            pending_lease_revocations: Vec::new(),
            token_overrides: DesignTokenMap::new(),
            projection_transports: HashMap::new(),
        }
    }

    /// Route `projection_id` to `transport`. `InProcess` (the default) is stored
    /// as an explicit entry so a later `transport()` reflects the last routing.
    fn set_transport(&mut self, projection_id: &str, transport: PortalTransport) {
        self.projection_transports
            .insert(projection_id.to_string(), transport);
    }

    /// The routed transport for `projection_id`, or the default `InProcess`.
    fn transport(&self, projection_id: &str) -> PortalTransport {
        self.projection_transports
            .get(projection_id)
            .copied()
            .unwrap_or_default()
    }

    fn resolve_visual_tokens(&self) -> tze_hud_projection::resident_grpc::PortalVisualTokens {
        let resolved =
            tze_hud_config::tokens::resolve_tokens(&DesignTokenMap::new(), &self.token_overrides);
        portal_visual_tokens_from_part_tokens(&resolve_portal_tokens(&resolved))
    }

    fn attach(&mut self, projection_id: &str, lease_id: Vec<u8>) {
        let tokens = self.resolve_visual_tokens();
        let config = ResidentGrpcPortalConfig::new(lease_id);
        let adapter = ResidentGrpcPortalAdapter::with_tokens(config, tokens);
        self.entries.insert(
            projection_id.to_string(),
            DriveEntry {
                adapter,
                scene_lease_id: None,
                tile_scene_id: None,
                prev_content_height_px: 0.0,
                hud_disconnected: false,
                agent_liveness_degraded: false,
                needs_degraded_repaint: false,
                activity_cue_clear_due_us: None,
                activity_cue_carried_unread: 0,
            },
        );
    }

    fn detach(&mut self, projection_id: &str) {
        // Drop the transport routing so a re-attach starts from the default
        // (hud-g7ool); the tombstone to the bridge is sent by the driver's
        // `detach_projection` before this runs.
        self.projection_transports.remove(projection_id);
        if let Some(entry) = self.entries.remove(projection_id) {
            if let Some(tile_id) = entry.tile_scene_id {
                self.pending_tile_removals.push(tile_id);
            }
            if let Some(lease_id) = entry.scene_lease_id {
                self.pending_lease_revocations.push(lease_id);
            }
        }
    }

    /// Remove a projection whose surface has ALREADY been removed from the scene
    /// (e.g. reaped by lease-grace expiry). Unlike [`Self::detach`], this does
    /// NOT queue a tile removal — the tile is already gone, so queuing one would
    /// only produce a spurious "failed to remove" warning on the next drain.
    fn forget(&mut self, projection_id: &str) {
        self.projection_transports.remove(projection_id);
        self.entries.remove(projection_id);
    }

    fn apply_token_map(&mut self, overrides: DesignTokenMap) {
        self.token_overrides = overrides;
        let tokens = self.resolve_visual_tokens();
        for entry in self.entries.values_mut() {
            entry.adapter.set_visual_tokens(tokens.clone());
        }
    }

    fn drain_pending_tile_removals(&mut self) -> Vec<SceneId> {
        self.pending_tile_removals.drain(..).collect()
    }

    fn drain_pending_lease_revocations(&mut self) -> Vec<SceneId> {
        self.pending_lease_revocations.drain(..).collect()
    }
}

/// In-process projection authority driver.
///
/// Hosts a `ProjectionAuthority` inside the runtime process and runs the portal
/// drain loop on each `about_to_wait` call. See module-level doc for the full
/// architecture description.
pub struct InProcessPortalDriver {
    authority: ProjectionAuthority,
    drive: InProcessPortalDriveState,
    /// Test-only observation alias for the most recently granted projection
    /// lease. Production lifecycle decisions use only the owning DriveEntry.
    #[cfg(test)]
    lease_id: Option<SceneId>,
    /// Publish-to-present latency bucket (hud-ttq97).
    ///
    /// Accumulates one sample per coalesced `RenderPortal` drain where
    /// `submitted_at_us > 0`.  Each sample is:
    ///
    /// ```text
    /// delta_us = now_us (drain wall-clock) − submitted_at_us (publish wall-clock)
    /// ```
    ///
    /// This measures the end-to-end time from when a portal update was first
    /// submitted (via `PublishOutput`) to when the drain loop materialises it
    /// for presentation — the primary latency signal for live task 5.7
    /// (hud-sonj6).
    ///
    /// Accessible via [`InProcessPortalDriver::portal_publish_to_present_latency`].
    portal_publish_to_present_latency: LatencyBucket,
    /// Count of drain cycles where the rate-window had not yet elapsed (hud-bq0gl.14).
    ///
    /// Incremented each time `take_due_portal_update` returns `Ok(None)` (the portal
    /// has a pending coalesced snapshot but its per-portal rate window has not elapsed
    /// yet).  A rising deferral count indicates the drain loop is being called more
    /// frequently than the rate window, or that portals are publishing faster than the
    /// rate window allows them to drain.
    ///
    /// Exposed via [`InProcessPortalDriver::drain_deferral_count`].
    drain_deferral_count: u64,
    /// Optional channel to the resident gRPC portal bridge (hud-d7frs, routing
    /// reworked in hud-g7ool).
    ///
    /// When set (production: only when the resident gRPC bridge is explicitly
    /// enabled via config), it is the transport for projections routed to
    /// [`PortalTransport::ResidentGrpcBridge`]: their coalesced state is forwarded
    /// as [`BridgeMessage::Publish`] and their in-process direct-scene
    /// materialisation is SUPPRESSED, so each bridged projection is materialised
    /// exactly once (over an authenticated gRPC `HudSession` stream) rather than
    /// double-painted. Projection removal sends a [`BridgeMessage::Detach`]
    /// tombstone so the bridge tears down the remote portal too. The send is
    /// non-blocking (`try_send`): a full channel drops the snapshot rather than
    /// stalling the winit thread. `None` (the default) leaves every projection on
    /// the in-process path, so the live path is byte-for-byte unchanged when the
    /// bridge is off.
    resident_grpc_bridge_tx: Option<tokio::sync::mpsc::Sender<BridgeMessage>>,
}

impl InProcessPortalDriver {
    /// Create a new in-process portal driver with default bounds.
    pub fn new() -> Self {
        let authority = ProjectionAuthority::new(ProjectionBounds::default())
            .expect("in-process projection authority initialisation must succeed");
        Self {
            authority,
            drive: InProcessPortalDriveState::new(),
            #[cfg(test)]
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        }
    }

    /// Return a reference to the publish-to-present latency bucket (hud-ttq97).
    ///
    /// Each sample is the elapsed microseconds from `submitted_at_us` (the
    /// wall-clock time when `PublishOutput` was called) to `now_us` (the
    /// wall-clock time when the drain loop materialised the coalesced update).
    ///
    /// Only `RenderPortal` drains where `submitted_at_us > 0` contribute a
    /// sample; `CreatePortalTile` drains and updates with an unset submission
    /// time (0) are excluded.
    pub fn portal_publish_to_present_latency(&self) -> &LatencyBucket {
        &self.portal_publish_to_present_latency
    }

    /// Number of drain cycles where the rate-window had not yet elapsed (hud-bq0gl.14).
    ///
    /// Incremented each time the drain loop encounters a portal whose pending
    /// coalesced snapshot cannot yet be materialised because the per-portal rate
    /// window has not elapsed.  See [`Self::portal_publish_to_present_latency`]
    /// for end-to-end latency.
    pub fn drain_deferral_count(&self) -> u64 {
        self.drain_deferral_count
    }

    /// Earliest wall-clock instant at which an idle event loop must drive this
    /// portal authority again: cadence release, activity-cue quiescence,
    /// agent-liveness transition, or a projection lease's orphan-grace/TTL
    /// boundary.
    pub(super) fn next_wake_deadline(&self, scene: &SceneGraph) -> Option<PortalWakeDeadline> {
        let now_us = now_wall_us();
        // Queued cleanup or degraded repaint must win over any stale timed
        // deadline. A missed scene-lock turn needs an immediate retry; sorting
        // it by wall time could otherwise attribute and delay that retry behind
        // an already-overdue liveness/TTL deadline.
        if self.has_immediate_work(scene) {
            return Some(PortalWakeDeadline {
                wall_us: now_us,
                family: PortalDeadlineFamily::ImmediateWork,
            });
        }
        let cadence = self.authority.next_pending_due_at_us(now_us);
        let agent_liveness = self.authority.next_agent_liveness_deadline_wall_us();
        let pending_input_expiry = self.authority.next_pending_input_expiry_wall_us();
        let activity = self
            .drive
            .entries
            .values()
            .filter_map(|entry| entry.activity_cue_clear_due_us)
            .min();
        let lease = self
            .drive
            .entries
            .values()
            .filter_map(|entry| entry.scene_lease_id)
            .filter_map(|lease_id| scene.leases.get(&lease_id))
            .filter_map(|lease| match lease.state {
                LeaseState::Active | LeaseState::Requested => (lease.ttl_ms > 0).then(|| {
                    lease
                        .granted_at_ms
                        .saturating_add(lease.ttl_ms)
                        .saturating_mul(1_000)
                }),
                LeaseState::Orphaned => {
                    let ttl = (lease.ttl_ms > 0).then(|| {
                        lease
                            .granted_at_ms
                            .saturating_add(lease.ttl_ms)
                            .saturating_mul(1_000)
                    });
                    let grace = lease.disconnected_at_ms.map(|at| {
                        at.saturating_add(lease.grace_period_ms)
                            .saturating_mul(1_000)
                    });
                    ttl.into_iter().chain(grace).min()
                }
                LeaseState::Suspended => lease.suspended_at_ms.map(|at| {
                    at.saturating_add(SceneGraph::DEFAULT_MAX_SUSPENSION_MS)
                        .saturating_mul(1_000)
                }),
                LeaseState::Denied
                | LeaseState::Revoked
                | LeaseState::Expired
                | LeaseState::Released => None,
            })
            .min();
        [
            cadence.map(|wall_us| PortalWakeDeadline {
                wall_us,
                family: PortalDeadlineFamily::Cadence,
            }),
            activity.map(|wall_us| PortalWakeDeadline {
                wall_us,
                family: PortalDeadlineFamily::ActivityCue,
            }),
            agent_liveness.map(|wall_us| PortalWakeDeadline {
                wall_us,
                family: PortalDeadlineFamily::AgentLiveness,
            }),
            pending_input_expiry.map(|wall_us| PortalWakeDeadline {
                wall_us,
                family: PortalDeadlineFamily::PendingInputExpiry,
            }),
            lease.map(|wall_us| PortalWakeDeadline {
                wall_us,
                family: PortalDeadlineFamily::LeaseLifecycle,
            }),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|deadline| (deadline.wall_us, deadline.family))
    }

    /// Whether scene access is required again even if no timed portal deadline
    /// or producer event remains. This keeps cleanup/degraded/reconciliation
    /// work from stranding after a try-lock miss.
    fn has_immediate_work(&self, scene: &SceneGraph) -> bool {
        !self.drive.pending_tile_removals.is_empty()
            || !self.drive.pending_lease_revocations.is_empty()
            || self.drive.entries.values().any(|entry| {
                entry.needs_degraded_repaint
                    || entry.scene_lease_id.is_some_and(|lease_id| {
                        scene
                            .leases
                            .get(&lease_id)
                            .is_none_or(|lease| lease.state.is_terminal())
                    })
            })
    }

    /// Install (or clear) the resident gRPC portal bridge channel (hud-d7frs).
    ///
    /// Installing the channel makes the resident gRPC bridge *available* as a
    /// transport, but does not by itself route any projection to it: per-projection
    /// routing is set via [`Self::set_projection_transport`]. A projection routed
    /// to [`PortalTransport::ResidentGrpcBridge`] is then materialised solely over
    /// the bridge (its in-process direct-scene path suppressed); projections left
    /// on the default `InProcess` transport are unaffected. `None` clears the
    /// channel and forces every projection back onto the in-process path.
    pub fn set_resident_grpc_bridge_tx(
        &mut self,
        tx: Option<tokio::sync::mpsc::Sender<BridgeMessage>>,
    ) {
        self.resident_grpc_bridge_tx = tx;
    }

    /// Route `projection_id` to a materialisation transport (hud-g7ool).
    ///
    /// This is the per-projection transport-selection seam: routing a projection
    /// to [`PortalTransport::ResidentGrpcBridge`] suppresses its in-process
    /// direct-scene materialisation and makes the bridge its sole materialiser
    /// (requires a bridge channel installed via [`Self::set_resident_grpc_bridge_tx`];
    /// otherwise it falls back to in-process — see [`Self::effective_transport`]).
    /// Routing back to `InProcess` (the default) restores the direct-scene path.
    pub fn set_projection_transport(&mut self, projection_id: &str, transport: PortalTransport) {
        self.drive.set_transport(projection_id, transport);
    }

    /// The transport that will actually materialise `projection_id` this drain
    /// (hud-g7ool).
    ///
    /// Resolves the routed transport but fails SAFE: a projection routed to the
    /// bridge only materialises over the bridge when a live channel is installed
    /// and open; if the channel is absent or its receiver has been dropped (the
    /// bridge task exited), the projection falls back to the in-process path so it
    /// still materialises somewhere rather than vanishing. This also guarantees
    /// the two transports remain mutually exclusive: the tee fires iff this returns
    /// `ResidentGrpcBridge`, and the in-process path runs iff it returns
    /// `InProcess`.
    fn effective_transport(&self, projection_id: &str) -> PortalTransport {
        match self.drive.transport(projection_id) {
            PortalTransport::ResidentGrpcBridge => match &self.resident_grpc_bridge_tx {
                Some(tx) if !tx.is_closed() => PortalTransport::ResidentGrpcBridge,
                _ => PortalTransport::InProcess,
            },
            PortalTransport::InProcess => PortalTransport::InProcess,
        }
    }

    /// Attach a new projection session to the driver.
    ///
    /// Called when an LLM agent attaches a projection.  The `lease_id` is
    /// passed through to the `ResidentGrpcPortalAdapter` so that the resident
    /// gRPC proto messages carry the correct lease identity.
    pub fn attach_projection(&mut self, projection_id: &str, lease_id: Vec<u8>) {
        self.drive.attach(projection_id, lease_id);
    }

    /// Detach a projection session from the driver.
    ///
    /// If the projection was materialised via the resident gRPC bridge, a
    /// [`BridgeMessage::Detach`] tombstone is sent FIRST so the bridge tears down
    /// the remote portal too (hud-sjdkk, absorbed here). Without it, in-process
    /// cleanup would remove the local drive entry while the bridge — which only
    /// ever sees positive snapshots — kept a STALE remote portal alive until its
    /// lease expired. The transport is read before `drive.detach` clears the
    /// routing. Non-bridged projections are unaffected.
    pub fn detach_projection(&mut self, projection_id: &str) {
        if self.effective_transport(projection_id) == PortalTransport::ResidentGrpcBridge {
            if let Some(tx) = &self.resident_grpc_bridge_tx {
                let _ = tx.try_send(BridgeMessage::Detach {
                    projection_id: projection_id.to_string(),
                });
            }
        }
        self.drive.detach(projection_id);
    }

    /// Latch a single attached projection's upstream as ungracefully dropped
    /// (hud-5i16d), flipping it to the degraded treatment.
    ///
    /// This is the production caller for [`ProjectionAuthority::mark_hud_disconnected`]
    /// that the disconnect/degraded failure-UX needs but never had — without it
    /// an ungraceful adapter/session drop left the surface looking live. Unlike a
    /// clean `Detach`/`Cleanup` (which removes the drive entry and queues tile
    /// removal), a drop **retains** the entry and its scene tile so the last
    /// coherent transcript window stays on screen under the degraded treatment
    /// (Portal Disconnect Presentation: dim + stale marker, no blanking). Only
    /// the authority's connection latch flips, so the next
    /// `projected_portal_state` carries `connection_degraded = true`.
    ///
    /// Idempotent: a projection already latched as disconnected is left untouched
    /// so repeated drop signals do not advance the disconnect timestamp. Returns
    /// `true` iff this call flipped a live projection to disconnected.
    ///
    /// Returns `false` for an unknown projection_id — a cleanly-detached
    /// projection is already gone from the drive map, so a late drop signal can
    /// never resurrect a degraded surface for it (AC: clean detach does not
    /// degrade).
    pub fn mark_projection_disconnected(&mut self, projection_id: &str) -> bool {
        self.mark_projection_disconnected_at(projection_id, now_wall_us())
    }

    /// Latch **all** attached projections as ungracefully dropped (hud-5i16d).
    ///
    /// Called when the shared upstream feeding the in-process driver closes
    /// without per-projection clean `Detach` ops — e.g. the MCP `portal_op`
    /// channel disconnects because its ingress task died
    /// (`windowed/portal.rs::drain_portal_ops`). Cleanly-detached projections are
    /// already absent from the drive map and so are unaffected.
    pub fn mark_all_projections_disconnected(&mut self) {
        let now = now_wall_us();
        let ids: Vec<String> = self.drive.entries.keys().cloned().collect();
        for id in ids {
            self.mark_projection_disconnected_at(&id, now);
        }
    }

    /// Timestamp-injecting core of [`Self::mark_projection_disconnected`].
    ///
    /// Production callers pass `now_wall_us()`; tests pass a deterministic value
    /// so the disconnect timestamp is reproducible (mirrors `drain`/`drain_inner`).
    fn mark_projection_disconnected_at(&mut self, projection_id: &str, now_wall_us: u64) -> bool {
        let Some(entry) = self.drive.entries.get_mut(projection_id) else {
            return false;
        };
        if entry.hud_disconnected {
            return false;
        }
        // `mark_hud_disconnected` rejects a zero timestamp; clamp to 1 to stay
        // well-defined even if the wall clock reads epoch.
        match self
            .authority
            .mark_hud_disconnected(projection_id, now_wall_us.max(1))
        {
            Ok(()) => {
                entry.hud_disconnected = true;
                // Request a forced degraded repaint (hud-h3mvo): a pure drop
                // adds no coalescer update, so without this the due-loop never
                // revisits the tile and it stays un-dimmed until the next
                // publish. The post-due-loop pass in `drain_inner` consumes it.
                entry.needs_degraded_repaint = true;
                tracing::info!(
                    proj_id = %projection_id,
                    "portal: upstream dropped ungracefully — connection marked degraded"
                );
                true
            }
            Err(error) => {
                tracing::warn!(
                    proj_id = %projection_id,
                    ?error,
                    "portal: mark_hud_disconnected rejected — session gone, dropping drive entry"
                );
                self.detach_projection(projection_id);
                false
            }
        }
    }

    /// Clear a previously-latched ungraceful disconnect on the next owner traffic
    /// (publish / re-attach) — the reconnect/resume signal (hud-5i16d).
    ///
    /// `record_hud_connection` restores `hud_connection = Some(..)`, which is the
    /// only thing that clears the authority's `connection_degraded` latch (an
    /// owner publish alone does not — see `projected_portal_state`). The
    /// synthesized metadata identifies the in-process MCP ingress as the
    /// connection; a stable `connection_id` keeps `record_hud_connection` from
    /// resetting the advisory lease, while the `now_wall_us`-derived
    /// `last_reconnect_wall_us` lets the authority count genuine reconnects.
    ///
    /// No-op unless the projection is currently latched as disconnected, so
    /// normal (non-recovery) publishes never touch the reconnect bookkeeping.
    fn clear_projection_disconnect_at(&mut self, projection_id: &str, now_wall_us: u64) {
        let Some(entry) = self.drive.entries.get_mut(projection_id) else {
            return;
        };
        if !entry.hud_disconnected {
            return;
        }
        let now = now_wall_us.max(1);
        let metadata = HudConnectionMetadata {
            connection_id: format!("mcp-portal:{projection_id}"),
            authenticated_session_id: format!("mcp-portal:{projection_id}"),
            granted_capabilities: Vec::new(),
            connected_at_wall_us: now,
            last_reconnect_wall_us: now,
        };
        match self
            .authority
            .record_hud_connection(projection_id, metadata)
        {
            Ok(()) => {
                entry.hud_disconnected = false;
                // The reconnect signal arrives with owner traffic (publish /
                // re-attach), which produces a due update that re-renders the
                // tile un-dimmed via the normal path — so any pending forced
                // degraded repaint is now moot (hud-h3mvo).
                entry.needs_degraded_repaint = false;
                tracing::info!(
                    proj_id = %projection_id,
                    "portal: upstream reconnected — connection restored, degraded cleared"
                );
            }
            Err(error) => {
                tracing::warn!(
                    proj_id = %projection_id,
                    ?error,
                    "portal: record_hud_connection rejected on reconnect — degraded latch retained"
                );
            }
        }
    }

    /// Get a mutable reference to the hosted [`ProjectionAuthority`].
    ///
    /// Used by the gRPC session layer to dispatch operations (Attach,
    /// PublishOutput, Detach, etc.) into the authority.
    pub fn authority_mut(&mut self) -> &mut ProjectionAuthority {
        &mut self.authority
    }

    /// Push a geometry snapshot to the projection session that owns `tile_id`.
    ///
    /// Called by the window management layer after a hotkey resize or pointer-
    /// affordance resize updates tile bounds in the scene (§6b.4: geometry-
    /// snapshot producer wiring, hud-npq6g).
    ///
    /// The drive entries are searched by `tile_scene_id` to find the owning
    /// `projection_id`, then `ProjectionAuthority::push_geometry_snapshot` is
    /// called so the geometry batch is visible to the drain loop via
    /// `projected_portal_state`.
    ///
    /// A projection routed to [`PortalTransport::ResidentGrpcBridge`] is
    /// materialised solely over the resident gRPC bridge and has no in-process
    /// tile (`entry.tile_scene_id` stays `None` — the visible tile lives in the
    /// wire session server's scene instead), so the reverse-lookup above cannot
    /// find it. `bridged_portal_id_hint` — the resized tile's declared
    /// `PortalSurface::identity.session_id`, read from the scene by the caller
    /// while it already held the scene lock — is decoded back to a
    /// `projection_id` via [`tze_hud_projection::projection_id_from_portal_id`]
    /// as a fallback, so a bridged (first-class-surface) portal's resize still
    /// reaches `session.latest_geometry` instead of leaving every subsequent
    /// republish on the adapter's declared attach-time bounds (hud-s62vv).
    ///
    /// Returns `true` if a matching session was found and the snapshot was
    /// accepted (sequence is strictly newer than any existing batch entry).
    /// Returns `false` if no session owns `tile_id` or the snapshot is stale.
    pub fn push_geometry_snapshot_for_tile(
        &mut self,
        tile_id: SceneId,
        snapshot: tze_hud_input::GeometrySnapshot,
        bridged_portal_id_hint: Option<&str>,
    ) -> bool {
        // Reverse-lookup: find the projection_id whose drive entry owns this tile.
        let projection_id = self
            .drive
            .entries
            .iter()
            .find(|(_, entry)| entry.tile_scene_id == Some(tile_id))
            .map(|(id, _)| id.clone())
            .or_else(|| {
                let candidate =
                    tze_hud_projection::projection_id_from_portal_id(bridged_portal_id_hint?)?;
                self.drive
                    .entries
                    .contains_key(candidate)
                    .then(|| candidate.to_string())
            });

        let Some(projection_id) = projection_id else {
            // No portal session is attached to this tile — nothing to do.
            return false;
        };

        let adapter_snapshot = AdapterGeometrySnapshot {
            rect: AdapterPortalRect::from_f32(
                snapshot.rect.x,
                snapshot.rect.y,
                snapshot.rect.width,
                snapshot.rect.height,
            ),
            gesture_active: snapshot.gesture_active,
            sequence: snapshot.sequence,
        };

        self.authority
            .push_geometry_snapshot(&projection_id, adapter_snapshot)
    }

    /// Route a focused portal composer batch into the owning projection state.
    ///
    /// The gRPC input-event broadcast is still emitted by `windowed`, but a
    /// cooperative projection owner polls input from [`ProjectionAuthority`],
    /// not from that namespace-keyed broadcast bus. This bridge is the
    /// production submit path: it resolves the focused portal tile back to the
    /// attached projection, lets the resident adapter consume the draft batch,
    /// and maps any transactional submission into the authority pending-input
    /// queue.
    ///
    /// Returns `None` when `tile_id` is not owned by an attached in-process
    /// projection, or when the batch contains no transactional submission.
    pub fn submit_composer_batch_for_tile(
        &mut self,
        tile_id: SceneId,
        batch: &DraftNotificationBatch,
        submitted_at_wall_us: u64,
        expires_at_wall_us: Option<u64>,
        content_classification: ContentClassification,
    ) -> Option<PortalInputFeedback> {
        let projection_id = self
            .drive
            .entries
            .iter()
            .find(|(_, entry)| entry.tile_scene_id == Some(tile_id))
            .map(|(projection_id, _)| projection_id.clone())?;

        let adapter_batch = adapter_draft_batch_from_runtime(batch);
        let entry = self.drive.entries.get_mut(&projection_id)?;
        entry.adapter.consume_draft_batch(&adapter_batch);

        let submission = adapter_batch.submission.as_ref()?;
        let result = entry.adapter.submit_composer_text(
            &mut self.authority,
            &projection_id,
            submission.text.clone(),
            submitted_at_wall_us,
            expires_at_wall_us,
            content_classification,
        );
        Some(result.feedback)
    }

    /// Ingest a composer submission that arrived over the resident gRPC bridge for
    /// `projection_id` (hud-omfqi).
    ///
    /// A bridged portal tile is materialised by the bridge's own gRPC session, so
    /// its viewer keystrokes never reach [`Self::submit_composer_batch_for_tile`]
    /// (which resolves by in-process tile id). Instead the bridge routes the
    /// submitted text back here, and this routes it through the SAME adapter →
    /// [`ProjectionAuthority`] sink a non-bridged submission reaches (echo viewer
    /// entry + enqueue pending input), so the driving session sees it.
    ///
    /// Returns `None` when no projection with an adapter is attached for
    /// `projection_id` (already detached).
    pub fn ingest_bridged_composer_submit(
        &mut self,
        projection_id: &str,
        text: String,
        submitted_at_wall_us: u64,
        content_classification: ContentClassification,
    ) -> Option<PortalInputFeedback> {
        let entry = self.drive.entries.get_mut(projection_id)?;
        let result = entry.adapter.submit_composer_text(
            &mut self.authority,
            projection_id,
            text,
            submitted_at_wall_us,
            None,
            content_classification,
        );
        Some(result.feedback)
    }

    /// Apply a new design-token override map, propagating to all live adapters.
    ///
    /// On a design-token / profile hot-reload this re-skins both transport
    /// families: the in-process adapters (via `drive.apply_token_map`) AND, when a
    /// resident gRPC bridge is installed, the bridged portals — the bridge holds
    /// its own adapters spawned with the startup tokens, so it needs the swap
    /// forwarded explicitly (`BridgeMessage::SetVisualTokens`) to reach parity
    /// with in-process surfaces (hud-fm0nf; builds on ygtiy's spawn-time
    /// `resolve_bridge_visual_tokens`, reused here to re-resolve from the same map).
    pub fn apply_token_map(&mut self, overrides: DesignTokenMap) {
        if let Some(tx) = &self.resident_grpc_bridge_tx {
            let tokens = crate::portal_tokens::resolve_bridge_visual_tokens(&overrides);
            // Best-effort, latest-wins: a full channel means a newer token swap is
            // already queued, so dropping this one is harmless.
            let _ = tx.try_send(BridgeMessage::SetVisualTokens(Box::new(tokens)));
        }
        self.drive.apply_token_map(overrides);
    }

    /// Dispatch one [`PortalOp`] message received from the MCP channel.
    ///
    /// Called from `windowed.rs::drain_portal_ops()` on the event-loop thread,
    /// once per message before the normal `drain()` call.  Replies are sent
    /// back through the one-shot channel embedded in each variant.
    ///
    /// ## `Attach` behaviour
    ///
    /// 1. Parses optional identity fields (`provider_kind`, `content_classification`)
    ///    from snake_case strings into projection enums. Missing fields default
    ///    safely (`provider_kind` → `other`; `content_classification` → `private`).
    ///    An unrecognized enum value is rejected via the reply channel before the
    ///    authority is called. String-typed hints (`workspace_hint`,
    ///    `repository_hint`, `icon_profile_hint`, `hud_target`) are forwarded as-is.
    /// 2. Calls `ProjectionAuthority::handle_attach` with a generated envelope.
    /// 3. On success, calls `self.attach_projection` so the drive state is ready
    ///    for the upcoming `drain()` iteration.
    /// 4. Returns the owner token through the reply channel.
    ///
    /// ## `PublishOutput` behaviour
    ///
    /// 1. Parses the optional `output_kind` / `content_classification`
    ///    snake_case strings into the projection enums (defaulting to
    ///    `assistant` / `private` when omitted) and threads through the
    ///    optional `coalesce_key`. An unrecognized enum value is rejected via
    ///    the reply channel before the authority is called.
    /// 2. Calls `ProjectionAuthority::handle_publish_output` with a generated
    ///    envelope and the caller-supplied owner token.
    /// 3. Forwards accept/reject through the reply channel.
    ///    The cadence coalescer inside the authority accumulates the update; the
    ///    normal `drain()` call in the same `about_to_wait` iteration (or the
    ///    next one) materialises it into the scene.
    pub fn dispatch_portal_op(&mut self, op: PortalOp) {
        self.dispatch_portal_op_at(op, now_wall_us());
    }

    /// [`Self::dispatch_portal_op`] at a caller-supplied wall-clock instant, so
    /// tests can drive time-dependent behaviour deterministically.
    fn dispatch_portal_op_at(&mut self, op: PortalOp, now_us: u64) {
        match op {
            PortalOp::List { reply } => {
                let request = ListProjectionsRequest {
                    request_id: uuid::Uuid::now_v7().to_string(),
                    client_timestamp_wall_us: now_us.max(1),
                };
                let response = self.authority.handle_list_projections(
                    request,
                    MCP_PORTAL_CALLER_IDENTITY,
                    now_us,
                );
                if response.accepted {
                    let batch = ProjectionListBatch {
                        projections: response
                            .projections
                            .into_iter()
                            .map(|projection| ProjectionListEntry {
                                projection_id: projection.projection_id,
                                display_name: projection.display_name,
                                lifecycle_state: lifecycle_state_wire(projection.lifecycle_state),
                                unread_output_count: projection.unread_output_count,
                                pending_input_count: projection.pending_input_count,
                            })
                            .collect(),
                    };
                    tracing::debug!(
                        projection_count = batch.projections.len(),
                        "portal_op: List accepted"
                    );
                    let _ = reply.send(Ok(batch));
                } else {
                    let error_code = response
                        .error_code
                        .unwrap_or(ProjectionErrorCode::ProjectionInternalError);
                    tracing::warn!(
                        error_code = %error_code,
                        "portal_op: List denied"
                    );
                    let _ = reply.send(Err(PortalOpRejection::new(
                        error_code,
                        response.status_summary,
                    )));
                }
            }

            PortalOp::Attach {
                projection_id,
                display_name,
                idempotency_key,
                provider_kind,
                content_classification,
                workspace_hint,
                repository_hint,
                icon_profile_hint,
                hud_target,
                reply,
            } => {
                let provider_kind = match parse_provider_kind(provider_kind.as_deref()) {
                    Ok(kind) => kind,
                    Err(reason) => {
                        let _ = reply.send(Err(PortalOpRejection::new(
                            ProjectionErrorCode::ProjectionInvalidArgument,
                            reason,
                        )));
                        return;
                    }
                };
                let content_classification =
                    match parse_content_classification(content_classification.as_deref()) {
                        Ok(classification) => classification,
                        Err(reason) => {
                            let _ = reply.send(Err(PortalOpRejection::new(
                                ProjectionErrorCode::ProjectionInvalidArgument,
                                reason,
                            )));
                            return;
                        }
                    };
                let request_id = uuid::Uuid::now_v7().to_string();
                let req = AttachRequest {
                    envelope: OperationEnvelope {
                        operation: ProjectionOperation::Attach,
                        projection_id: projection_id.clone(),
                        request_id,
                        client_timestamp_wall_us: now_us.max(1),
                    },
                    provider_kind,
                    display_name,
                    workspace_hint,
                    repository_hint,
                    icon_profile_hint,
                    content_classification,
                    hud_target,
                    idempotency_key,
                };
                let resp = self
                    .authority
                    .handle_attach(req, MCP_PORTAL_CALLER_IDENTITY, now_us);
                if resp.accepted {
                    // Wire the drive state so the drain loop can materialise tiles.
                    //
                    // Guard against idempotent re-attach: `handle_attach` returns
                    // `accepted=true` for a replay (same projection_id + matching
                    // idempotency_key) without re-creating the session. Re-running
                    // `attach_projection` here would `insert` a fresh `DriveEntry`
                    // (adapter with `tile_id() == None`, `tile_scene_id == None`),
                    // so the next drain would treat the portal as new and create a
                    // duplicate tile — orphaning/leaking the existing one. Only wire
                    // a drive entry the first time we see this projection_id.
                    if !self.drive.entries.contains_key(&projection_id) {
                        self.attach_projection(&projection_id, Vec::new());
                        // Route the new projection onto the resident gRPC bridge
                        // when it is installed (hud-hfuxy). The bridge-enabled
                        // config/env knob (`resident_grpc_bridge_tx.is_some()`) is
                        // the only production signal that exists for "materialise
                        // portals over the bridge" — there is no per-projection
                        // selector yet (reserved for the external-authority epic;
                        // see hud-g7ool's design note) — so this is the minimal
                        // wiring that actually uses the hud-g7ool discriminant.
                        // Before this, nothing in production ever called
                        // `set_projection_transport`, so an enabled bridge stayed
                        // materialised-but-inert (every projection defaulted to
                        // `InProcess` forever). `effective_transport` fails back to
                        // `InProcess` if the channel later closes, so this stays
                        // safe even if the bridge task has already exited by drain
                        // time. When the bridge is not installed (default
                        // deployment), this is a no-op — byte-for-byte unchanged.
                        if self.resident_grpc_bridge_tx.is_some() {
                            self.set_projection_transport(
                                &projection_id,
                                PortalTransport::ResidentGrpcBridge,
                            );
                        }
                    }
                    // Re-attach is the reconnect signal: if this projection was
                    // latched as ungracefully disconnected, restore the connection
                    // so the degraded treatment clears (hud-5i16d). A no-op for a
                    // first attach or a still-live projection.
                    self.clear_projection_disconnect_at(&projection_id, now_us);
                    tracing::info!(
                        proj_id = %projection_id,
                        "portal_op: Attach accepted — drive entry ensured"
                    );
                    let token = resp.owner_token.unwrap_or_default();
                    let _ = reply.send(Ok(token));
                } else {
                    let error_code = resp
                        .error_code
                        .unwrap_or(ProjectionErrorCode::ProjectionInternalError);
                    tracing::warn!(
                        proj_id = %projection_id,
                        error_code = %error_code,
                        "portal_op: Attach denied"
                    );
                    let _ =
                        reply.send(Err(PortalOpRejection::new(error_code, resp.status_summary)));
                }
            }

            PortalOp::PublishOutput {
                projection_id,
                owner_token,
                output_text,
                logical_unit_id,
                output_kind,
                content_classification,
                coalesce_key,
                expects_reply,
                reply,
            } => {
                // Parse the optional snake_case classification strings into the
                // projection enums. Omitted fields default safely
                // (assistant / private — privacy is safe-by-default). An
                // unrecognized value is rejected before the authority sees it.
                let output_kind = match parse_output_kind(output_kind.as_deref()) {
                    Ok(kind) => kind,
                    Err(reason) => {
                        let _ = reply.send(Err(PortalOpRejection::new(
                            ProjectionErrorCode::ProjectionInvalidArgument,
                            reason,
                        )));
                        return;
                    }
                };
                let content_classification =
                    match parse_content_classification(content_classification.as_deref()) {
                        Ok(classification) => classification,
                        Err(reason) => {
                            let _ = reply.send(Err(PortalOpRejection::new(
                                ProjectionErrorCode::ProjectionInvalidArgument,
                                reason,
                            )));
                            return;
                        }
                    };
                let request_id = uuid::Uuid::now_v7().to_string();
                let req = PublishOutputRequest {
                    envelope: OperationEnvelope {
                        operation: ProjectionOperation::PublishOutput,
                        projection_id: projection_id.clone(),
                        request_id,
                        client_timestamp_wall_us: now_us.max(1),
                    },
                    owner_token,
                    output_text,
                    output_kind,
                    content_classification,
                    logical_unit_id,
                    coalesce_key,
                    expects_reply: expects_reply.unwrap_or(false),
                };
                let resp =
                    self.authority
                        .handle_publish_output(req, MCP_PORTAL_CALLER_IDENTITY, now_us);
                if resp.accepted {
                    // A successful owner publish after an ungraceful drop is the
                    // reconnect signal: restore the connection so the degraded
                    // treatment clears (hud-5i16d). Guarded to a no-op unless the
                    // projection is currently latched disconnected, so normal
                    // publishes never perturb the reconnect bookkeeping.
                    self.clear_projection_disconnect_at(&projection_id, now_us);
                    tracing::debug!(
                        proj_id = %projection_id,
                        coalesced = resp.coalesced_output_count,
                        "portal_op: PublishOutput accepted"
                    );
                    let _ = reply.send(Ok(()));
                } else {
                    let error_code = resp
                        .error_code
                        .unwrap_or(ProjectionErrorCode::ProjectionInternalError);
                    tracing::warn!(
                        proj_id = %projection_id,
                        error_code = %error_code,
                        "portal_op: PublishOutput denied"
                    );
                    let _ =
                        reply.send(Err(PortalOpRejection::new(error_code, resp.status_summary)));
                }
            }

            PortalOp::PublishStatus {
                projection_id,
                owner_token,
                lifecycle_state,
                status_text,
                reply,
            } => {
                // Parse the snake_case lifecycle string before the authority sees
                // it. There is no default — the lifecycle state is the payload of
                // publish_status — so an unrecognized value is rejected with the
                // stable invalid-argument code.
                let lifecycle_state = match parse_lifecycle_state(&lifecycle_state) {
                    Ok(state) => state,
                    Err(reason) => {
                        let _ = reply.send(Err(PortalOpRejection::new(
                            ProjectionErrorCode::ProjectionInvalidArgument,
                            reason,
                        )));
                        return;
                    }
                };
                let request_id = uuid::Uuid::now_v7().to_string();
                let req = PublishStatusRequest {
                    envelope: OperationEnvelope {
                        operation: ProjectionOperation::PublishStatus,
                        projection_id: projection_id.clone(),
                        request_id,
                        client_timestamp_wall_us: now_us.max(1),
                    },
                    owner_token,
                    lifecycle_state,
                    status_text,
                };
                let resp =
                    self.authority
                        .handle_publish_status(req, MCP_PORTAL_CALLER_IDENTITY, now_us);
                if resp.accepted {
                    // Echo the authority's applied lifecycle state back so the MCP
                    // caller can observe the round-trip. Fall back to the request
                    // state if the response omits it (it always sets it on accept).
                    let applied = resp.lifecycle_state.unwrap_or(lifecycle_state);
                    tracing::debug!(
                        proj_id = %projection_id,
                        lifecycle = ?applied,
                        "portal_op: PublishStatus accepted"
                    );
                    let _ = reply.send(Ok(lifecycle_state_wire(applied)));
                } else {
                    let error_code = resp
                        .error_code
                        .unwrap_or(ProjectionErrorCode::ProjectionInternalError);
                    tracing::warn!(
                        proj_id = %projection_id,
                        error_code = %error_code,
                        "portal_op: PublishStatus denied"
                    );
                    let _ =
                        reply.send(Err(PortalOpRejection::new(error_code, resp.status_summary)));
                }
            }

            PortalOp::GetPendingInput {
                projection_id,
                owner_token,
                max_items,
                max_bytes,
                reply,
            } => {
                let request_id = uuid::Uuid::now_v7().to_string();
                let req = GetPendingInputRequest {
                    envelope: OperationEnvelope {
                        operation: ProjectionOperation::GetPendingInput,
                        projection_id: projection_id.clone(),
                        request_id,
                        client_timestamp_wall_us: now_us.max(1),
                    },
                    owner_token,
                    max_items,
                    max_bytes,
                };
                let resp = self.authority.handle_get_pending_input(
                    req,
                    MCP_PORTAL_CALLER_IDENTITY,
                    now_us,
                );
                if resp.accepted {
                    let items = resp
                        .pending_input
                        .iter()
                        .map(pending_input_entry_from_item)
                        .collect();
                    let batch = PendingInputBatch {
                        items,
                        remaining_count: resp.pending_remaining_count,
                        remaining_bytes: resp.pending_remaining_bytes,
                    };
                    tracing::debug!(
                        proj_id = %projection_id,
                        delivered = resp.pending_input.len(),
                        remaining = resp.pending_remaining_count,
                        "portal_op: GetPendingInput accepted"
                    );
                    let _ = reply.send(Ok(batch));
                } else {
                    let error_code = resp
                        .error_code
                        .unwrap_or(ProjectionErrorCode::ProjectionInternalError);
                    tracing::warn!(
                        proj_id = %projection_id,
                        error_code = %error_code,
                        "portal_op: GetPendingInput denied"
                    );
                    let _ =
                        reply.send(Err(PortalOpRejection::new(error_code, resp.status_summary)));
                }
            }

            PortalOp::AcknowledgeInput {
                projection_id,
                owner_token,
                input_id,
                ack_state,
                ack_message,
                not_before_wall_us,
                reply,
            } => {
                let ack_state = match parse_ack_state(&ack_state) {
                    Ok(state) => state,
                    Err(reason) => {
                        let _ = reply.send(Err(PortalOpRejection::new(
                            ProjectionErrorCode::ProjectionInvalidArgument,
                            reason,
                        )));
                        return;
                    }
                };
                let request_id = uuid::Uuid::now_v7().to_string();
                let req = AcknowledgeInputRequest {
                    envelope: OperationEnvelope {
                        operation: ProjectionOperation::AcknowledgeInput,
                        projection_id: projection_id.clone(),
                        request_id,
                        client_timestamp_wall_us: now_us.max(1),
                    },
                    owner_token,
                    input_id,
                    ack_state,
                    ack_message,
                    not_before_wall_us,
                };
                let resp = self.authority.handle_acknowledge_input(
                    req,
                    MCP_PORTAL_CALLER_IDENTITY,
                    now_us,
                );
                if resp.accepted {
                    tracing::debug!(
                        proj_id = %projection_id,
                        "portal_op: AcknowledgeInput accepted"
                    );
                    let _ = reply.send(Ok(()));
                } else {
                    let error_code = resp
                        .error_code
                        .unwrap_or(ProjectionErrorCode::ProjectionInternalError);
                    tracing::warn!(
                        proj_id = %projection_id,
                        error_code = %error_code,
                        "portal_op: AcknowledgeInput denied"
                    );
                    let _ =
                        reply.send(Err(PortalOpRejection::new(error_code, resp.status_summary)));
                }
            }

            PortalOp::Hold {
                projection_id,
                owner_token,
                ttl_ms,
                reply,
            } => {
                let hold_until_wall_us = if ttl_ms == 0 {
                    u64::MAX
                } else {
                    now_us.saturating_add(ttl_ms.saturating_mul(1_000))
                };
                let result = self.authority.handle_hold(
                    &projection_id,
                    &owner_token,
                    hold_until_wall_us,
                    now_us,
                );
                if let Err(error_code) = result {
                    tracing::warn!(
                        proj_id = %projection_id,
                        error_code = %error_code,
                        "portal_op: Hold denied"
                    );
                }
                let _ = reply.send(result.map_err(|code| {
                    PortalOpRejection::new(code, "portal hold denied".to_string())
                }));
            }

            PortalOp::Detach {
                projection_id,
                owner_token,
                reason,
                reply,
            } => {
                let request_id = uuid::Uuid::now_v7().to_string();
                let req = DetachRequest {
                    envelope: OperationEnvelope {
                        operation: ProjectionOperation::Detach,
                        projection_id: projection_id.clone(),
                        request_id,
                        client_timestamp_wall_us: now_us.max(1),
                    },
                    owner_token,
                    reason,
                };
                let resp = self
                    .authority
                    .handle_detach(req, MCP_PORTAL_CALLER_IDENTITY, now_us);
                if resp.accepted {
                    // The authority purged its session + coalescer entry. Drop
                    // the driver-side drive entry / tile mapping so the next
                    // drain does not attempt to render a removed surface and the
                    // projection_id is free to be re-attached cleanly.
                    self.detach_projection(&projection_id);
                    tracing::info!(
                        proj_id = %projection_id,
                        "portal_op: Detach accepted — drive entry dropped"
                    );
                    let _ = reply.send(Ok(()));
                } else {
                    let error_code = resp
                        .error_code
                        .unwrap_or(ProjectionErrorCode::ProjectionInternalError);
                    tracing::warn!(
                        proj_id = %projection_id,
                        error_code = %error_code,
                        "portal_op: Detach denied"
                    );
                    let _ =
                        reply.send(Err(PortalOpRejection::new(error_code, resp.status_summary)));
                }
            }

            PortalOp::Cleanup {
                projection_id,
                cleanup_authority,
                owner_token,
                operator_authority,
                reason,
                reply,
            } => {
                let cleanup_authority = match parse_cleanup_authority(&cleanup_authority) {
                    Ok(authority) => authority,
                    Err(reason) => {
                        let _ = reply.send(Err(PortalOpRejection::new(
                            ProjectionErrorCode::ProjectionInvalidArgument,
                            reason,
                        )));
                        return;
                    }
                };
                let request_id = uuid::Uuid::now_v7().to_string();
                let req = CleanupRequest {
                    envelope: OperationEnvelope {
                        operation: ProjectionOperation::Cleanup,
                        projection_id: projection_id.clone(),
                        request_id,
                        client_timestamp_wall_us: now_us.max(1),
                    },
                    cleanup_authority,
                    owner_token,
                    operator_authority,
                    reason,
                };
                let resp = self
                    .authority
                    .handle_cleanup(req, MCP_PORTAL_CALLER_IDENTITY, now_us);
                if resp.accepted {
                    // Cleanup purges authority state through either owner-token or
                    // operator-authority paths. Drop driver-side state as well so
                    // stale projected-session tiles are not rendered after cleanup.
                    self.detach_projection(&projection_id);
                    tracing::info!(
                        proj_id = %projection_id,
                        "portal_op: Cleanup accepted — drive entry dropped"
                    );
                    let _ = reply.send(Ok(()));
                } else {
                    let error_code = resp
                        .error_code
                        .unwrap_or(ProjectionErrorCode::ProjectionInternalError);
                    tracing::warn!(
                        proj_id = %projection_id,
                        error_code = %error_code,
                        "portal_op: Cleanup denied"
                    );
                    let _ =
                        reply.send(Err(PortalOpRejection::new(error_code, resp.status_summary)));
                }
            }
        }
    }

    /// Run the work-conserving portal drain loop and apply results to the scene.
    ///
    /// Called from `about_to_wait` after composer-draft flush. This is the
    /// production follow-tail wiring point (hud-2iup7).
    ///
    /// # Parameters
    ///
    /// - `scene`: mutable reference to the scene graph (acquired by the caller
    ///   with `try_lock`; not acquired here to avoid blocking the main thread).
    /// - `input_processor`: mutable reference to the main-thread `InputProcessor`.
    /// - `tab_id`: the active tab in which new portal tiles should be created.
    ///   If `None` (no active tab), `CreatePortalTile` drains are skipped.
    ///
    /// # Panic safety
    ///
    /// The drain body is wrapped in `catch_unwind`. A panic resets drive state
    /// and logs an error without propagating to the event loop.
    pub fn drain(
        &mut self,
        scene: &mut SceneGraph,
        input_processor: &mut InputProcessor,
        tab_id: Option<SceneId>,
    ) {
        let now_us = now_wall_us();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.drain_inner(scene, input_processor, tab_id, now_us)
        }));
        if let Err(payload) = result {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic payload".to_string()
            };
            tracing::error!(
                error = %msg,
                "portal projection driver drain panicked — drive state reset"
            );
            // Reset all drive entries to prevent stale state accumulation.
            self.drive = InProcessPortalDriveState::new();
        }
    }

    /// Find the tab a new portal tile should be hosted in, **without activating
    /// it** (hud-zccuf).
    ///
    /// Returns the ID of the lowest-`display_order` existing tab, or `None` if
    /// there are no tabs at all.
    ///
    /// This is the read-only first step of the two-phase tab resolution used by
    /// the `CreatePortalTile` drain arm.  The caller is responsible for calling
    /// `switch_active_tab` on the returned ID — but only **after** both
    /// `ensure_projection_lease` and `create_tile` have succeeded. Deferring the
    /// switch ensures that a failing cycle leaves `scene.active_tab` unchanged
    /// (the projection stays attached and self-heals on its next publish).
    ///
    /// When this returns `None` (no tabs exist), the caller must create a
    /// default "Main" tab via `scene.create_tab`; that call auto-activates the
    /// new tab when `active_tab` is `None` (SceneGraph semantics), so no
    /// deferred switch is needed for that path.
    ///
    /// # Known Limitation
    ///
    /// `scene.active_tab == None` is ambiguous: the boot case (default tab has
    /// no widgets) and an operator deliberately blanking the HUD both present
    /// identically.  Safe mode (RFC 0008 §3.4) is the v1 coarse suppression
    /// mechanism.  A scene-level suppression flag (Option A in
    /// `docs/design/portal-operator-blank-hud-suppression.md`) is deferred to
    /// hud-yafc7.
    fn find_portal_host_tab(scene: &SceneGraph) -> Option<SceneId> {
        scene
            .tabs
            .values()
            .min_by_key(|t| t.display_order)
            .map(|t| t.id)
    }

    /// Inner drain implementation.
    ///
    /// `now_us` is the caller-supplied wall-clock timestamp in microseconds
    /// since UNIX epoch.  Production callers pass `now_wall_us()`; tests pass a
    /// deterministic value so time-dependent behaviour is reproducible without
    /// wall-clock coupling.
    fn drain_inner(
        &mut self,
        scene: &mut SceneGraph,
        input_processor: &mut InputProcessor,
        tab_id: Option<SceneId>,
        now_us: u64,
    ) {
        self.drain_pending_tile_removals(scene);

        // Pending viewer input has an authority-owned TTL independent of owner
        // polling. Expire due items first and enqueue a coalesced state update so
        // a parked static portal repaints its count/bytes/delivery cue exactly at
        // the boundary.
        self.authority.sweep_pending_input_expiry(now_us.max(1));

        // Agent-side liveness sweep (hud-ccj2o). Authentication recency lives
        // in the projection authority; presentation and removal stay runtime-
        // owned. Synchronize degradation and recovery before lease handling so
        // a recovered owner can reactivate an orphaned lease before rendering.
        self.authority
            .sweep_agent_liveness_degradation(now_us.max(1));
        let liveness_states: Vec<(String, bool)> = self
            .drive
            .entries
            .keys()
            .map(|id| (id.clone(), self.authority.is_agent_liveness_degraded(id)))
            .collect();
        for (projection_id, degraded) in liveness_states {
            let Some(entry) = self.drive.entries.get_mut(&projection_id) else {
                continue;
            };
            if entry.agent_liveness_degraded == degraded {
                continue;
            }
            entry.agent_liveness_degraded = degraded;
            entry.needs_degraded_repaint = true;
            if degraded {
                entry.activity_cue_clear_due_us = None;
            }
        }

        // Resume-side of the lease-grace lifecycle (hud-i429x): if an owner
        // returned within grace, reconnect its orphaned lease BEFORE the
        // render due-loop below, so a resumed publish paints under an Active lease
        // (`set_tile_root_checked` → `require_active_lease`) instead of being
        // rejected and dropped. The orphan + reap half runs at the end of drain.
        self.reconnect_lease_grace_on_resume(scene);

        let policy = ProjectedPortalPolicy::permit_all();
        // Local counters for this drain cycle — emitted as a tracing event after
        // the loop for portal health observability (hud-bq0gl.14).
        let mut cycle_updates: u32 = 0;
        let mut cycle_deferrals: u32 = 0;
        // Per-cycle iteration guard (hud-bsr7u). Bounds the number of loop
        // iterations so a session/coalescer divergence can never busy-spin the
        // event loop (which runs this drain under the scene lock).
        let mut iterations: u32 = 0;

        loop {
            // Defense-in-depth backstop: never iterate more than the cap per tick.
            // Reaching this indicates a divergence bug (the loop should normally
            // be work-conserving and terminate when the coalescer is drained).
            if iterations >= MAX_PORTAL_DRAIN_ITERATIONS_PER_CYCLE {
                tracing::error!(
                    iterations,
                    cap = MAX_PORTAL_DRAIN_ITERATIONS_PER_CYCLE,
                    "portal drain hit per-cycle iteration cap — aborting tick to \
                     avoid wedging the event loop (likely session/coalescer divergence)"
                );
                break;
            }
            iterations = iterations.saturating_add(1);

            // Round-robin fairness oracle (tasks.md §5.1 / §5.4).
            let Some(proj_id) = self.authority.next_due_projection_id() else {
                break;
            };

            // Materialise the coalesced update for this portal.
            let update = match self.authority.take_due_portal_update(&proj_id, now_us) {
                Ok(Some(update)) => update,
                Ok(None) => {
                    // Rate-window not yet elapsed — count and exit the loop.
                    self.drain_deferral_count = self.drain_deferral_count.saturating_add(1);
                    cycle_deferrals = cycle_deferrals.saturating_add(1);
                    break;
                }
                Err(_) => {
                    // Projection not found or expired. `take_due_portal_update`
                    // returns early on the session lookup BEFORE it consumes the
                    // coalescer entry, so an orphaned entry (session gone but
                    // coalescer still pending — e.g. operator cleanup, hud-bsr7u)
                    // would otherwise be returned again by the next
                    // `next_due_projection_id` and busy-spin this loop forever.
                    // Discard the coalescer entry here so it cannot recur, then
                    // clean up the adapter.
                    self.authority.discard_portal_coalescer_entry(&proj_id);
                    self.detach_projection(&proj_id);
                    continue;
                }
            };

            // Build the full projected portal state for rendering.
            let Some(mut state) = self.authority.projected_portal_state(&proj_id, &policy) else {
                // Session was removed between take_due and state query (race).
                self.detach_projection(&proj_id);
                continue;
            };

            // Ambient unread-output-count indicator liveness (hud-meqet).
            // `take_due_portal_update` above zeroed `session.unread_output_count`
            // as part of consuming the drain, so `projected_portal_state` read a
            // stale 0 — `Some(0)` renders nothing, suppressing the indicator on
            // EVERY real coalesced drain (the production drain-then-render order).
            // Carry the just-drained batch's unread count from `update` into the
            // render state so the indicator is live for all downstream paths: the
            // resident-gRPC bridge tee (below, clones `state`) and the in-process
            // `render_batch` create/render arms.
            state.unread_output_count =
                carry_drained_unread_count(state.unread_output_count, update.unread_output_count);
            // In-transcript unread-divider liveness (hud-n95zc). Same ordering
            // hazard as the ambient count above, one field over:
            // `projected_portal_state` derived `visible_unread_output_count` from
            // the same now-zeroed `session.unread_output_count`, so
            // `unread_divider_boundary` (resident_grpc.rs) saw `Some(0)` on every
            // real drain and the divider never rendered in production — only the
            // ambient count had a carry-forward fix (hud-meqet). Under the
            // `permit_all()` policy every production call site uses, no viewer
            // clearance filters any unit out of `visible_transcript`, so the
            // just-drained raw count is exactly the correct clearance-corrected
            // value here too — the same substitution `carry_drained_unread_count`
            // already performs for the ambient field.
            state.visible_unread_output_count = carry_drained_unread_count(
                state.visible_unread_output_count,
                update.unread_output_count,
            );

            // One-shot cue-quiesce scheduling (hud-kbm80). The agent-activity /
            // streaming-cursor cue is derived from the newest transcript unit's
            // appended-at vs the render `now`, but the round-robin due-loop only
            // revisits a portal on a fresh coalescer update — so after the terminal
            // append on an otherwise-idle portal the cue would persist past its
            // window and misrepresent ongoing activity. Record the instant past
            // which the cue reads false against THIS materialised state; the
            // post-due-loop pass repaints once at/after it so the cue clears without
            // external traffic. Overwritten every materialisation, so a fresh append
            // simply extends the deadline and a now-quiesced state clears it. Set
            // before the transport branch so it applies to both the in-process tiled
            // path and the bridged tee.
            if let Some(entry) = self.drive.entries.get_mut(&proj_id) {
                entry.activity_cue_clear_due_us = activity_cue_clear_due_us(&state, now_us);
                // Stash the just-drained raw unread count so the forced quiesce
                // repaint below can restore the ambient "N unread" indicator: it
                // re-derives state from the now-zeroed session and would otherwise
                // drop the count even though no viewer action cleared it (hud-kbm80).
                entry.activity_cue_carried_unread = update.unread_output_count;
            }

            // Per-projection transport routing (hud-g7ool). A projection routed to
            // the resident gRPC bridge is materialised SOLELY by the bridge:
            // forward its coalesced state as a `Publish` and SUPPRESS the in-process
            // direct-scene path below, so the two transports never double-paint one
            // scene (the original hud-d7frs double-materialisation bug). The send is
            // non-blocking so the winit drain never stalls on a slow/full bridge; a
            // dropped snapshot is acceptable (state is coalesced/latest-relevant).
            // Non-bridged projections (the default, and the shipped config) fall
            // through to the unchanged in-process path and are never teed.
            if self.effective_transport(&proj_id) == PortalTransport::ResidentGrpcBridge {
                // The bridge owns the visible tile, but the runtime still owns
                // stale-surface governance. A tile-less per-projection lease
                // supplies the same SceneGraph grace clock as the in-process
                // path without creating a second timer.
                if self.ensure_projection_lease(&proj_id, scene).is_none() {
                    tracing::warn!(
                        proj_id = %proj_id,
                        "portal drain: could not obtain bridge governance lease"
                    );
                    continue;
                }
                if let Some(tx) = &self.resident_grpc_bridge_tx {
                    let _ = tx.try_send(BridgeMessage::Publish {
                        projection_id: proj_id.clone(),
                        state: Box::new(state.clone()),
                    });
                }
                // The bridge is this projection's materialiser — count the update
                // for the drain-health metric and skip the in-process arms below.
                cycle_updates = cycle_updates.saturating_add(1);
                continue;
            }

            // Check if the drive entry exists and what kind of command to issue.
            // We do this before taking a mutable borrow of drive.entries so that
            // `ensure_projection_lease` (which borrows `self` mutably) can be called
            // without a concurrent mutable borrow through `entry`.
            let entry_exists = self.drive.entries.contains_key(&proj_id);
            if !entry_exists {
                // No drive entry (non-portal surface). Skip silently.
                continue;
            }
            let needs_create = self
                .drive
                .entries
                .get(&proj_id)
                .map(|e| e.adapter.tile_id().is_none())
                .unwrap_or(false);

            // Determine command kind: CreatePortalTile if tile not yet created.
            let command_kind = if needs_create {
                ResidentGrpcPortalCommandKind::CreatePortalTile
            } else {
                ResidentGrpcPortalCommandKind::RenderPortal
            };

            match command_kind {
                ResidentGrpcPortalCommandKind::CreatePortalTile => {
                    // A portal tile needs a host tab. Cooperative projections must
                    // render even when no tab is active (hud-obw3q): a config whose
                    // default tab carries no widgets boots with `active_tab == None`
                    // (windowed/lifecycle.rs), and `take_due_portal_update` above has
                    // already consumed this projection's coalesced update — so the
                    // old `continue` here dropped the published content AND never
                    // created a tile, leaving an accepted publish silently invisible.
                    //
                    // Tab activation is DEFERRED (hud-zccuf): we find/create the
                    // host tab here but only call `switch_active_tab` after both
                    // `ensure_projection_lease` and `create_tile` succeed. A failing
                    // cycle must not leave a premature `active_tab` mutation — the
                    // projection stays attached and self-heals on its next publish.
                    let pending_tab_activation; // committed in the Ok arm below
                    let host_tab = if let Some(already_active) = tab_id {
                        // Outer frame already has an active tab — use it directly,
                        // no deferred switch needed.
                        pending_tab_activation = None;
                        Some(already_active)
                    } else if let Some(candidate) = Self::find_portal_host_tab(scene) {
                        // An existing tab is present but not yet active.  Note it
                        // for deferred activation; do NOT mutate active_tab here.
                        // NOTE: fires for both the boot case and an operator-blanked HUD
                        // — see find_portal_host_tab's KNOWN LIMITATION doc + hud-yafc7.
                        pending_tab_activation = Some(candidate);
                        Some(candidate)
                    } else {
                        // No tabs at all: create a default one.  `create_tab`
                        // auto-activates when `active_tab` is `None` (SceneGraph
                        // semantics), so no deferred switch is needed for this path.
                        pending_tab_activation = None;
                        scene.create_tab("Main", 0).ok()
                    };
                    let active_tab = match host_tab {
                        Some(t) => t,
                        None => {
                            // Only reachable if there are no tabs AND `create_tab`
                            // failed (e.g. tab budget exhausted).  Rare; the update
                            // is lost this cycle but the projection stays attached
                            // and retries on its next publish.
                            tracing::warn!(
                                proj_id = %proj_id,
                                "portal drain: no tab available and could not create \
                                 one — CreatePortalTile deferred"
                            );
                            continue;
                        }
                    };

                    // Ensure the driver has an active lease in the scene.
                    // Must be called BEFORE taking entry borrow to avoid borrow conflict.
                    let lease_id = match self.ensure_projection_lease(&proj_id, scene) {
                        Some(id) => id,
                        None => {
                            tracing::warn!(
                                proj_id = %proj_id,
                                "portal drain: could not obtain projection lease — \
                                 CreatePortalTile deferred"
                            );
                            continue;
                        }
                    };

                    // Now take the mutable entry borrow after lease acquisition.
                    let Some(entry) = self.drive.entries.get_mut(&proj_id) else {
                        continue;
                    };

                    // Derive initial bounds from the adapter's configured viewport.
                    // Use expanded dimensions (720×360) as the default starting rect;
                    // subsequent geometry snapshots will provide accurate live dimensions.
                    let viewport_h = entry.adapter.config_viewport_height(state.presentation);
                    // Width falls back to a reasonable default (720px expanded, 420px
                    // collapsed). We derive it from the height ratio to stay consistent with
                    // the adapter defaults without exposing config internals.
                    let viewport_w = match state.presentation {
                        tze_hud_projection::ProjectedPortalPresentation::Expanded => 720.0_f32,
                        tze_hud_projection::ProjectedPortalPresentation::Collapsed => 420.0_f32,
                    };
                    let bounds = Rect::new(0.0, 0.0, viewport_w, viewport_h);

                    match scene.create_tile(
                        active_tab,
                        PORTAL_DRIVER_NAMESPACE,
                        lease_id,
                        bounds,
                        PORTAL_Z_ORDER,
                    ) {
                        Ok(tile_scene_id) => {
                            // Register scroll config so follow-tail tracking works.
                            let _ = scene.register_tile_scroll_config(
                                tile_scene_id,
                                TileScrollConfig {
                                    scrollable_x: false,
                                    scrollable_y: true,
                                    content_width: None,
                                    content_height: None,
                                },
                            );

                            // Record the tile ID in the adapter (little-endian bytes per
                            // RFC 0001 §4.1 wire encoding).
                            let tile_id_le = tile_scene_id.to_bytes_le().to_vec();
                            entry.adapter.record_created_tile(tile_id_le);
                            entry.tile_scene_id = Some(tile_scene_id);

                            // Paint the first publish's content into the freshly
                            // created tile (hud-utbiy). The authority coalesces rapid
                            // publishes into one update, and this create drain has
                            // already consumed that update via `take_due_portal_update`
                            // above. Before this fix the arm fell through without
                            // rendering, so the coalesced transcript was lost and the
                            // tile stayed an empty grey rect forever. Now (the tile
                            // exists) the adapter renders the content batch and we
                            // apply it directly to the scene — the same content the
                            // gRPC family publishes over the wire.
                            match entry.adapter.render_batch_with_surface(&state, now_us) {
                                Ok(batch) => {
                                    tze_hud_protocol::convert::apply_portal_render_batch_to_scene(
                                        scene,
                                        tile_scene_id,
                                        PORTAL_DRIVER_NAMESPACE,
                                        &batch,
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        proj_id = %proj_id,
                                        tile_id = ?tile_scene_id,
                                        error = ?e,
                                        "portal drain: CreatePortalTile — render_batch failed; \
                                         tile created but content not painted"
                                    );
                                }
                            }

                            // Tile creation succeeded — commit the deferred tab
                            // activation now (hud-zccuf).  For the `tab_id.is_some()`
                            // and `create_tab` paths `pending_tab_activation` is
                            // `None` so this is a no-op in those cases.
                            if let Some(tab_to_activate) = pending_tab_activation {
                                let _ = scene.switch_active_tab(tab_to_activate);
                            }

                            tracing::debug!(
                                proj_id = %proj_id,
                                tile_id = ?tile_scene_id,
                                "portal drain: CreatePortalTile — tile created in scene"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                proj_id = %proj_id,
                                error = ?e,
                                "portal drain: CreatePortalTile scene creation failed"
                            );
                        }
                    }
                }

                ResidentGrpcPortalCommandKind::RenderPortal
                | ResidentGrpcPortalCommandKind::ReusePortalTile => {
                    // hud-2iup7: Wire notify_tile_content_appended (spec §3.2 / §3.3).
                    //
                    // The InputProcessor advances follow-tail when the tile is AtTail
                    // (spec §3.2) and preserves the offset when scrolled-back (spec §3.3).
                    // We only need to call it — the decision is fully encapsulated there.
                    let Some(entry) = self.drive.entries.get_mut(&proj_id) else {
                        continue;
                    };
                    let Some(tile_scene_id) = entry.tile_scene_id else {
                        tracing::warn!(
                            proj_id = %proj_id,
                            "portal drain: RenderPortal but tile_scene_id is None — skipping notify"
                        );
                        continue;
                    };

                    // Paint the coalesced update's content into the existing tile
                    // (hud-utbiy). The adapter renders the same MutationBatch the
                    // gRPC family publishes over the wire; we apply it directly to
                    // the scene. The geometry/scroll tracking below is preserved —
                    // it consumes the same `update` for follow-tail accounting.
                    match entry.adapter.render_batch_with_surface(&state, now_us) {
                        Ok(batch) => {
                            tze_hud_protocol::convert::apply_portal_render_batch_to_scene(
                                scene,
                                tile_scene_id,
                                PORTAL_DRIVER_NAMESPACE,
                                &batch,
                            );
                            // This render already reflects the current degraded
                            // state (render_batch reads `connection_degraded`),
                            // so any pending forced repaint is satisfied — clear
                            // it so the post-due-loop pass does not double-paint
                            // (hud-h3mvo).
                            entry.needs_degraded_repaint = false;
                        }
                        Err(e) => {
                            tracing::warn!(
                                proj_id = %proj_id,
                                tile_id = ?tile_scene_id,
                                error = ?e,
                                "portal drain: RenderPortal — render_batch failed; \
                                 content not repainted this cycle"
                            );
                        }
                    }

                    // Compute append geometry (mirrors drain_and_emit_portal_updates in
                    // projection_authority.rs §1316-1363).
                    //
                    // line_height_px  = transcript_font_size_px × PORTAL_LINE_HEIGHT_MULTIPLIER
                    // new_content_h   = total_rendered_lines × line_height_px
                    // viewport_h      = geometry_batch.latest.rect.height_px when present;
                    //                   else adapter configured bounds for current presentation
                    let line_height_px = entry.adapter.visual_tokens().transcript_font_size_px
                        * PORTAL_LINE_HEIGHT_MULTIPLIER;

                    // Count actual rendered lines across all visible units. A
                    // TranscriptUnit may contain embedded newlines, so we use
                    // `.lines().count().max(1)` per unit to avoid underestimation.
                    //
                    // §Viewer Reply Echo (two-pane INPUT/OUTPUT split): when
                    // Expanded, `portal_markdown_with` renders the INPUT band
                    // (label + `input_history` units + trailing blank) into the
                    // SAME single-node markdown as the OUTPUT transcript above —
                    // Collapsed never renders it. Omitting it here would undercount
                    // `new_content_height_px` whenever a viewer has reply history,
                    // breaking scroll clamp / follow-tail for long INPUT history.
                    let mut total_lines: usize = update
                        .visible_transcript
                        .iter()
                        .map(|unit| unit.output_text.lines().count().max(1))
                        .sum();
                    if matches!(
                        state.presentation,
                        tze_hud_projection::ProjectedPortalPresentation::Expanded
                    ) && !state.input_history.is_empty()
                    {
                        total_lines += 2; // input-history label line + trailing blank
                        total_lines += state
                            .input_history
                            .iter()
                            .map(|unit| unit.output_text.lines().count().max(1))
                            .sum::<usize>();
                    }
                    let total_lines = total_lines.max(1);

                    let new_content_height_px = total_lines as f32 * line_height_px;

                    // Prefer the live (transient) geometry snapshot, then the
                    // DURABLE resized bounds, and only then the adapter config.
                    // The transient geometry_batch is consumed after delivery,
                    // so a transcript append arriving AFTER a resize would
                    // otherwise compute follow-tail/max-scroll against the stale
                    // config height while the body renders at the resized height
                    // (hud-v4k1h: resized_bounds is the persistent size source).
                    let viewport_height_px = state
                        .geometry_batch
                        .as_ref()
                        .and_then(|gb| gb.latest)
                        .map(|snap| snap.rect.height_px as f32)
                        .or_else(|| state.resized_bounds.map(|r| r.height_px as f32))
                        .unwrap_or_else(|| {
                            entry.adapter.config_viewport_height(state.presentation)
                        });

                    // hud-ttq97: record publish-to-present latency for this portal drain.
                    //
                    // `submitted_at_us` is the wall-clock time when `PublishOutput` was
                    // called (set by the cadence coalescer in `record_append`).  `now_us`
                    // is the drain wall-clock time supplied by the caller.  The delta is
                    // the end-to-end publish→present elapsed time for this coalesced update.
                    //
                    // Guard: skip when `submitted_at_us == 0` (coalescer returned the
                    // `unwrap_or(0)` sentinel from `peek_submitted_at` — no submission
                    // timestamp was recorded yet) or when `now_us < submitted_at_us`
                    // (should not happen in production, but guards against test fixtures
                    // that supply out-of-order timestamps).
                    if update.submitted_at_us > 0 && now_us >= update.submitted_at_us {
                        let delta_us = now_us - update.submitted_at_us;
                        self.portal_publish_to_present_latency.record(delta_us);
                        tracing::trace!(
                            proj_id = %proj_id,
                            submitted_at_us = update.submitted_at_us,
                            now_us,
                            delta_us,
                            "portal drain: publish-to-present latency recorded"
                        );
                    }

                    // hud-pkg2g / hud-66i1s: detect head-trim and call
                    // notify_head_content_removed.
                    //
                    // A content-height decrease between consecutive `RenderPortal`
                    // drains indicates that content was removed from the head of the
                    // visible transcript.  Two trim sites produce this:
                    //   1. PortalCadenceCoalescer::record_append (64 KiB cap): drops
                    //      oldest bytes from the payload before storing the snapshot.
                    //   2. visible_transcript_window (16 KiB cap): slices the retained
                    //      transcript to the newest max_visible_transcript_bytes bytes.
                    //
                    // Detection criterion: new_content_height_px < prev_content_height_px.
                    // The height is the authoritative signal because the runtime caller
                    // uses new_content_height_px (from append_geometry) as the total
                    // content height for scroll accounting.  Any decrease — regardless
                    // of whether the byte count also decreased — means the caller must
                    // receive a notify_head_content_removed call BEFORE
                    // notify_tile_content_appended, so that
                    // ScrollTileState::total_content_height_px is correct when the
                    // follow-tail bound is recomputed (spec §3.3, hud-66i1s fix).
                    //
                    // The previous dual-condition (bytes_shrank && height_shrank) was
                    // overly restrictive: it would miss a height shrink that occurred
                    // without a corresponding byte-count decrease (e.g., a many-newline
                    // unit evicted by a flat unit of equal or greater byte count).  This
                    // mirrors the fix applied to projection_authority.rs in PR #779
                    // (hud-hkaw2).
                    let prev_height = entry.prev_content_height_px;
                    if new_content_height_px < prev_height {
                        let removed_px = prev_height - new_content_height_px;
                        let trim_changed =
                            input_processor.notify_head_content_removed(tile_scene_id, removed_px);
                        tracing::debug!(
                            proj_id = %proj_id,
                            tile_id = ?tile_scene_id,
                            removed_px,
                            prev_height_px = prev_height,
                            new_height_px = new_content_height_px,
                            scroll_adjusted = trim_changed,
                            "portal drain: head-trim detected — notify_head_content_removed"
                        );
                    }
                    // Update per-portal height tracking for the next drain cycle.
                    entry.prev_content_height_px = new_content_height_px;

                    // spec §3.2 / §3.3: call notify_tile_content_appended.
                    // - AtTail  → InputProcessor advances follow-tail (§3.2).
                    // - ScrolledBack → InputProcessor is a no-op (§3.3).
                    let changed = input_processor.notify_tile_content_appended(
                        tile_scene_id,
                        new_content_height_px,
                        viewport_height_px,
                        line_height_px,
                        scene,
                    );

                    tracing::trace!(
                        proj_id = %proj_id,
                        tile_id = ?tile_scene_id,
                        new_content_height_px,
                        viewport_height_px,
                        line_height_px,
                        scroll_advanced = changed,
                        "portal drain: RenderPortal — notify_tile_content_appended"
                    );

                    // hud-g1ena.3: carry the ambient unread-output count onto the
                    // tile so the compositor can render it as the badge the
                    // jump-to-latest pill MAY carry (portal-chat-grade-affordances
                    // §Jump-to-Latest Affordance). Uses the aggregate
                    // `unread_output_count` (carried past the drain-zero above,
                    // hud-meqet), matching the ambient in-transcript indicator. A
                    // redacted (`None`) or empty count stores 0 → no badge; the pill
                    // is itself gated on `scrolled_back`, so the badge clears with it
                    // the instant the viewer returns to the tail (local-first, no
                    // adapter round trip).
                    scene.set_tile_unread_count(
                        tile_scene_id,
                        state.unread_output_count.unwrap_or(0),
                    );

                    // §6b.4: consume the geometry batch after delivery so that
                    // `projected_portal_state` does not re-deliver the same snapshot
                    // on the next drain cycle. The caller (push_geometry_snapshot_for_tile)
                    // writes new snapshots; old ones must be cleared after being read.
                    if state.geometry_batch.is_some() {
                        self.authority.consume_geometry_batch(&proj_id);
                    }
                }

                ResidentGrpcPortalCommandKind::ReleaseLease => {
                    // ReleaseLease: no content notification needed.
                    tracing::debug!(
                        proj_id = %proj_id,
                        "portal drain: ReleaseLease — no notify required"
                    );
                }
            }

            // Count each successfully materialised update (hud-bq0gl.14).
            cycle_updates = cycle_updates.saturating_add(1);
        }

        // Forced degraded-repaint pass (hud-h3mvo).
        //
        // A pure upstream drop latches `connection_degraded` but enqueues no
        // coalescer update, so the round-robin due-loop above never revisits the
        // dropped portal and its tile keeps its live (un-dimmed) paint until the
        // next publish happens to arrive — the disconnect is invisible. Here we
        // repaint each flagged entry exactly once, under the same scene lock, so
        // a drop visibly dims within one frame without a subsequent publish. The
        // flag is one-shot (set on the disconnect transition, cleared here and on
        // any normal render / reconnect), so an idle degraded tile is not
        // re-rendered every drain.
        //
        // Bridged projections are admitted too (hud-vne15): a bridged projection is
        // materialised solely over the resident gRPC bridge and has no in-process
        // tile (`tile_scene_id.is_none()`), so the tile-only filter would exclude it
        // and its degraded state would never be forwarded to the bridge on a pure
        // drop — the remote portal would keep its live paint. A tile-less
        // in-process entry is also admitted long enough to consume its one-shot
        // flag: it has no surface to materialise, so retaining that flag would
        // manufacture permanent `ImmediateWork` retries after liveness/drop.
        let degraded_repaint_ids: Vec<String> = self
            .drive
            .entries
            .iter()
            .filter(|(_, entry)| entry.needs_degraded_repaint)
            .map(|(id, _)| id.clone())
            .collect();
        for proj_id in degraded_repaint_ids {
            let transport = self.effective_transport(&proj_id);
            if transport == PortalTransport::InProcess
                && self
                    .drive
                    .entries
                    .get(&proj_id)
                    .is_some_and(|entry| entry.tile_scene_id.is_none())
            {
                // There is no in-process tile to repaint. Clear this one-shot
                // obligation after the authoritative drain, while preserving the
                // first immediate wake that brought us here.
                if let Some(entry) = self.drive.entries.get_mut(&proj_id) {
                    entry.needs_degraded_repaint = false;
                }
                continue;
            }
            // Re-derive state so the repaint reflects the current (degraded)
            // connection latch. If the session vanished between the drop and
            // here, drop the now-orphaned drive entry instead of repainting.
            let Some(mut state) = self.authority.projected_portal_state(&proj_id, &policy) else {
                self.detach_projection(&proj_id);
                continue;
            };
            // Ambient + in-transcript unread liveness on a degraded repaint
            // (hud-n95zc, hud-lylbz). This re-derivation reads
            // `session.unread_output_count` fresh, which is a stale 0 whenever
            // the portal's own prior due-loop drain already zeroed it (idle
            // since, same premise the quiesce pass below relies on) — so both
            // `unread_output_count` (the ambient "N unread" ticker) and
            // `visible_unread_output_count` (the in-transcript divider) would
            // read `Some(0)` and vanish from an otherwise still-live degraded
            // repaint. Restore both from the last-materialised raw drained
            // count the same way the quiesce pass does, via
            // `carry_drained_unread_count`.
            if let Some(carried) = self
                .drive
                .entries
                .get(&proj_id)
                .map(|e| e.activity_cue_carried_unread)
            {
                state.unread_output_count =
                    carry_drained_unread_count(state.unread_output_count, carried);
                state.visible_unread_output_count =
                    carry_drained_unread_count(state.visible_unread_output_count, carried);
            }
            // Per-projection transport routing (hud-g7ool / hud-vne15): a bridged
            // projection's degraded state is forwarded over the bridge and its
            // in-process repaint suppressed, mirroring the due-loop rule (tee iff
            // bridged). A bridged projection has no in-process tile, so it now enters
            // this pass via the tile-OR-bridged filter above; forwarding the degraded
            // `ProjectedPortalState` as a `Publish` makes the remote portal reflect
            // the degraded treatment (the bridge materialises whatever state carries
            // `connection_degraded`). Clear the one-shot flag here: the in-process arm
            // below is what clears it for the tiled path, so without this a tile-less
            // bridged entry would re-tee a `Publish` every drain.
            if transport == PortalTransport::ResidentGrpcBridge {
                if let Some(entry) = self.drive.entries.get_mut(&proj_id) {
                    entry.needs_degraded_repaint = false;
                }
                if let Some(tx) = &self.resident_grpc_bridge_tx {
                    let _ = tx.try_send(BridgeMessage::Publish {
                        projection_id: proj_id.clone(),
                        state: Box::new(state.clone()),
                    });
                }
                continue;
            }
            let Some(entry) = self.drive.entries.get_mut(&proj_id) else {
                continue;
            };
            // Clear first: a render failure must not leave the entry flagged for
            // a retry on every subsequent drain (the next genuine publish will
            // repaint it regardless).
            entry.needs_degraded_repaint = false;
            let Some(tile_scene_id) = entry.tile_scene_id else {
                continue;
            };
            match entry.adapter.render_batch_with_surface(&state, now_us) {
                Ok(batch) => {
                    tze_hud_protocol::convert::apply_portal_render_batch_to_scene(
                        scene,
                        tile_scene_id,
                        PORTAL_DRIVER_NAMESPACE,
                        &batch,
                    );
                    tracing::debug!(
                        proj_id = %proj_id,
                        tile_id = ?tile_scene_id,
                        "portal drain: forced degraded repaint on pure drop (hud-h3mvo)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        proj_id = %proj_id,
                        tile_id = ?tile_scene_id,
                        error = ?e,
                        "portal drain: degraded repaint render_batch failed; \
                         tile not dimmed this cycle"
                    );
                }
            }
        }

        // Forced agent-activity cue-quiesce pass (hud-kbm80).
        //
        // The ambient "⋯ writing" header line + "▍" streaming cursor are derived
        // from the newest transcript unit's `appended_at_wall_us` vs the render
        // `now` (`resident_grpc::agent_activity_active`) and quiesce once that tail
        // ages past `PORTAL_ACTIVITY_QUIESCE_WINDOW_US`. But the round-robin due-loop
        // above only revisits a portal on a fresh coalescer update — so after the
        // terminal append on an otherwise-idle portal nothing re-renders it and the
        // cue persists indefinitely, misrepresenting ongoing activity
        // (portal-chat-grade-affordances §Agent Activity and Streaming Cue: the cue
        // SHALL "quiesce promptly once appends stop"). Each materialisation recorded
        // `activity_cue_clear_due_us` (the first instant the cue reads false); here
        // we force exactly one repaint per flagged entry at/after that instant, under
        // the same scene lock, so the cue clears with no external traffic. The
        // deadline is one-shot (cleared here; overwritten by any fresh append's
        // materialisation above), so a quiesced idle portal is not repainted every
        // drain.
        //
        // Bridged projections are admitted too (hud-vne15), mirroring the degraded
        // pass: a bridged projection has no in-process tile, so the tile-only filter
        // would exclude it and its cue would never quiesce on the remote portal.
        // Admit an entry when it has a tile OR is bridged; the loop re-tees the
        // quiesced state for the bridged case and repaints the tile for the
        // in-process case.
        let cue_quiesce_ids: Vec<String> = self
            .drive
            .entries
            .iter()
            .filter(|(id, e)| {
                e.activity_cue_clear_due_us.is_some_and(|due| now_us >= due)
                    && (e.tile_scene_id.is_some()
                        || self.effective_transport(id) == PortalTransport::ResidentGrpcBridge)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for proj_id in cue_quiesce_ids {
            // Re-derive state so the repaint reflects the current transcript (and
            // re-runs the cue derivation, which is now false past the deadline). If
            // the session vanished since the deadline was scheduled, drop the
            // now-orphaned drive entry instead of repainting.
            let Some(mut state) = self.authority.projected_portal_state(&proj_id, &policy) else {
                self.detach_projection(&proj_id);
                continue;
            };
            // Restore the ambient unread-output count the same way the normal drain
            // does (hud-kbm80): `take_due_portal_update` zeroed the session, so the
            // re-derived state reads a stale 0 and would drop the "N unread"
            // indicator on this cue-only repaint. Carry the last-materialised raw
            // count forward through `carry_drained_unread_count` (which preserves the
            // upstream redaction). On an idle portal (the quiesce premise) no viewer
            // action has changed the count since that materialisation, so it is still
            // accurate.
            if let Some(carried) = self
                .drive
                .entries
                .get(&proj_id)
                .map(|e| e.activity_cue_carried_unread)
            {
                state.unread_output_count =
                    carry_drained_unread_count(state.unread_output_count, carried);
                // In-transcript unread-divider liveness (hud-n95zc): same stale-0
                // race as the ambient count just above, for the field
                // `unread_divider_boundary` actually reads. Carried by the same
                // idle-premise argument documented above.
                state.visible_unread_output_count =
                    carry_drained_unread_count(state.visible_unread_output_count, carried);
            }
            // Per-projection transport routing (hud-g7ool / hud-vne15): a bridged
            // projection's quiesced state is forwarded over the bridge and its
            // in-process repaint suppressed, mirroring the degraded pass.
            if self.effective_transport(&proj_id) == PortalTransport::ResidentGrpcBridge {
                // Clear the one-shot deadline ONLY on a successful enqueue. Unlike a
                // normal publish, this quiesce `Publish` has no later update to
                // supersede it (idle portal by premise), so if the bounded bridge
                // channel is `Full` (bridge reconnecting / slow to drain) dropping it
                // would strand the remote portal's "⋯ writing" cue forever. On `Full`
                // we retain the deadline and let the next drain retry with freshly
                // re-derived (still-quiesced) state; on success — or a closed/absent
                // channel that will never accept it — we clear it (hud-kbm80).
                match &self.resident_grpc_bridge_tx {
                    Some(tx) => match tx.try_send(BridgeMessage::Publish {
                        projection_id: proj_id.clone(),
                        state: Box::new(state.clone()),
                    }) {
                        Ok(()) => {
                            if let Some(entry) = self.drive.entries.get_mut(&proj_id) {
                                entry.activity_cue_clear_due_us = None;
                            }
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            // Backpressure: retain the deadline; retry next drain.
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            // Bridge gone; nothing will ever accept this quiesce
                            // repaint, so clear to avoid an unbounded retry.
                            if let Some(entry) = self.drive.entries.get_mut(&proj_id) {
                                entry.activity_cue_clear_due_us = None;
                            }
                        }
                    },
                    None => {
                        // No bridge channel installed; clear to avoid an unbounded
                        // retry against a transport that cannot receive it.
                        if let Some(entry) = self.drive.entries.get_mut(&proj_id) {
                            entry.activity_cue_clear_due_us = None;
                        }
                    }
                }
                continue;
            }
            let Some(entry) = self.drive.entries.get_mut(&proj_id) else {
                continue;
            };
            // Clear first: a render failure must not re-flag the entry for a retry
            // on every subsequent drain (the deadline has passed; the derivation is
            // false now and the next genuine publish will repaint regardless).
            entry.activity_cue_clear_due_us = None;
            let Some(tile_scene_id) = entry.tile_scene_id else {
                continue;
            };
            match entry.adapter.render_batch_with_surface(&state, now_us) {
                Ok(batch) => {
                    tze_hud_protocol::convert::apply_portal_render_batch_to_scene(
                        scene,
                        tile_scene_id,
                        PORTAL_DRIVER_NAMESPACE,
                        &batch,
                    );
                    tracing::debug!(
                        proj_id = %proj_id,
                        tile_id = ?tile_scene_id,
                        "portal drain: forced agent-activity cue-quiesce repaint (hud-kbm80)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        proj_id = %proj_id,
                        tile_id = ?tile_scene_id,
                        error = ?e,
                        "portal drain: cue-quiesce repaint render_batch failed; \
                         cue not cleared this cycle"
                    );
                }
            }
        }

        // Lease-grace reaper (hud-i429x): bound an ungracefully-dropped portal's
        // degraded window by the lease grace and remove its surface on grace
        // expiry via the existing scene orphan path. Runs after the render passes
        // so a same-drain reconnect (op loop cleared `hud_disconnected`) is
        // observed as live here. Cheap map scans; reaping bumps the scene version
        // so the removal repaints even on an idle frame.
        self.sweep_lease_grace(scene);

        // Portal health snapshot — emitted at debug level after each drain cycle
        // (hud-bq0gl.14).  Carries:
        //   • per-cycle update/deferral counts
        //   • cumulative drain-deferral counter
        //   • coalescer fairness stats (portal_count, total_taken, total_coalesced)
        //   • publish-to-present latency percentiles (p50/p95/p99)
        //
        // Emit only when there was activity (at least one update or deferral) to
        // avoid spamming on idle drain cycles.
        if cycle_updates > 0 || cycle_deferrals > 0 {
            let lat = &self.portal_publish_to_present_latency;
            tracing::debug!(
                cycle_updates,
                cycle_deferrals,
                cumulative_deferrals = self.drain_deferral_count,
                coalescer_portal_count = self.authority.coalescer_portal_count(),
                coalescer_pending = self.authority.coalescer_pending_portal_count(),
                coalescer_total_taken = self.authority.coalescer_total_taken(),
                coalescer_total_coalesced = self.authority.coalescer_total_coalesced(),
                latency_p50_us = lat.p50(),
                latency_p95_us = lat.p95(),
                latency_p99_us = lat.p99(),
                latency_sample_count = lat.samples.len(),
                "portal drain: health snapshot"
            );
        }
    }

    /// Ensure one projection has an active lease in the scene graph.
    ///
    /// Returns the cached lease only if it is still **Active** (mutations
    /// allowed). An Orphaned lease remains authoritative until reconnect or
    /// expiry; terminal/missing leases are replaced and cached.
    ///
    /// The active-state check (not mere map presence) is load-bearing for the
    /// post-grace re-attach path: when a prior portal's lease orphans and its
    /// grace period elapses, `SceneGraph::expire_leases` removes the lease's
    /// tiles but leaves the lease entry resident in the map as `Expired`
    /// (terminal state is recorded in place, not pruned). A session attaching
    /// *after* that grace expiry must therefore start a FRESH portal under a new
    /// lease. If we reused the cached-but-Expired lease here, the subsequent
    /// `create_tile` would fail its `require_active_lease` check and the new
    /// session would silently get no portal — never reviving the removed surface
    /// nor presenting pre-death content as live, but also never coming back.
    fn ensure_projection_lease(
        &mut self,
        projection_id: &str,
        scene: &mut SceneGraph,
    ) -> Option<SceneId> {
        let cached = self.drive.entries.get(projection_id)?.scene_lease_id;
        if let Some(lease_id) = cached {
            // Reuse only if the cached lease is still Active. A terminal
            // (Expired/Revoked) or orphaned/suspended lease must NOT be reused —
            // `lease_capabilities` would still return `Some` for an Expired
            // lease left resident by grace-period reaping, so we check liveness.
            if scene.lease_is_active(&lease_id) {
                return Some(lease_id);
            }
            // Never replace an Orphaned lease while its stale-surface grace is
            // running. Authenticated recovery reconnects this exact lease at the
            // top of the drain; stale traffic must not obtain a fresh lease and
            // silently escape the existing grace bound.
            if scene.lease_is_orphaned(&lease_id) {
                return None;
            }
        }
        // Grant a new lease for the portal driver.
        // 24-hour TTL in milliseconds (long-lived resident service).
        let new_lease = scene.grant_lease(PORTAL_DRIVER_NAMESPACE, 86_400_000);
        self.drive.entries.get_mut(projection_id)?.scene_lease_id = Some(new_lease);
        #[cfg(test)]
        {
            // Test observation only; production lifecycle code never consults it.
            self.lease_id = Some(new_lease);
        }
        Some(new_lease)
    }

    /// Reconnect an orphaned projection lease when its owner returns within grace —
    /// run at the TOP of `drain_inner`, BEFORE the render due-loop (hud-i429x).
    ///
    /// Ordering is load-bearing: a resume publish is rendered by the due-loop via
    /// `set_tile_root_checked`, which calls `require_active_lease`. If the lease
    /// were still Orphaned at render time, that first resumed batch would be
    /// rejected and its coalesced update lost (the content would not paint until a
    /// later publish). So the lease must be Active before the due-loop runs. The
    /// re-attach/publish op already cleared `hud_disconnected` at op-dispatch time
    /// (`clear_projection_disconnect_at`, which runs in `dispatch_portal_op` before
    /// this drain), so a live entry here means an owner returned; reconnecting the
    /// lease restores mutability, clears the scene disconnection badge, and stops
    /// the grace clock so the reaper leaves the resumed surface alone
    /// (resume-within-grace, hud-xlx1r).
    ///
    /// Lease ownership is per projection: recovering one owner never reconnects
    /// a stale sibling's lease or extends that sibling's stale-content window.
    fn reconnect_lease_grace_on_resume(&mut self, scene: &mut SceneGraph) {
        let reconnects: Vec<(String, SceneId)> = self
            .drive
            .entries
            .iter()
            .filter_map(|(projection_id, entry)| {
                (!entry.hud_disconnected && !entry.agent_liveness_degraded)
                    .then_some(entry.scene_lease_id)
                    .flatten()
                    .filter(|lease_id| scene.lease_is_orphaned(lease_id))
                    .map(|lease_id| (projection_id.clone(), lease_id))
            })
            .collect();
        for (projection_id, lease_id) in reconnects {
            match scene.reconnect_lease(&lease_id, scene.now_millis()) {
                Ok(()) => tracing::info!(
                    proj_id = %projection_id,
                    "portal: projection lease reconnected within grace — resuming the same surface"
                ),
                Err(error) => tracing::warn!(
                    proj_id = %projection_id,
                    ?error,
                    "portal: failed to reconnect projection lease within grace"
                ),
            }
        }
    }

    /// Bound an ungracefully-dropped cooperative portal's degraded window by the
    /// lease grace, and remove its surface when grace expires (hud-i429x;
    /// openspec `portal-disconnect-resume-ux` §3.2 "staleness bounded by lease
    /// grace; grace expiry removes the surface").
    ///
    /// This is the production wiring §3.2 lacked. `mark_hud_disconnected`
    /// (hud-5i16d) opened the degraded window and dimmed the transcript, but
    /// nothing orphaned the scene lease or ran the reaper — so a dropped portal
    /// dimmed yet was NEVER grace-removed: an unbounded stale window. `expire_leases`
    /// / `disconnect_lease` had zero production callers before this.
    ///
    /// Runs at the END of every drain (after the render passes; the resume-side
    /// reconnect is handled up front by [`Self::reconnect_lease_grace_on_resume`]).
    /// The windowed scheduler uses [`Self::next_wake_deadline`] to wake
    /// exactly at the next per-projection grace boundary, so an otherwise-idle
    /// screen still reaps on time without periodic polling. The steps are cheap
    /// map scans, both no-ops on the common all-live path:
    ///
    /// 1. ORPHAN — each disconnected or agent-stale projection orphans its own
    ///    Active lease. In-process tiles receive the scene disconnection badge;
    ///    bridge-routed projections use a tile-less lease with the same clock.
    /// 2. REAP — expire each elapsed orphan lease, then remove that one projection
    ///    from authority and drive state. Bridge entries also emit a `Detach`
    ///    tombstone. A continuously-live projection's Active lease is never reaped
    ///    on its long resident TTL.
    fn sweep_lease_grace(&mut self, scene: &mut SceneGraph) {
        // Stamp lifecycle transitions with the scene's own clock so the grace
        // bound checked by `expire_lease` is in the same domain.
        let now_ms = scene.now_millis();

        let lifecycle: Vec<(String, SceneId, bool)> = self
            .drive
            .entries
            .iter()
            .filter_map(|(projection_id, entry)| {
                entry.scene_lease_id.map(|lease_id| {
                    (
                        projection_id.clone(),
                        lease_id,
                        entry.hud_disconnected || entry.agent_liveness_degraded,
                    )
                })
            })
            .collect();

        // 1. ORPHAN: each stale projection starts its own grace clock.
        for (projection_id, lease_id, stale) in &lifecycle {
            if !stale || !scene.lease_is_active(lease_id) {
                continue;
            }
            match scene.disconnect_lease(lease_id, now_ms) {
                Ok(()) => tracing::info!(
                    proj_id = %projection_id,
                    "portal: projection lease orphaned on ungraceful drop — \
                     degraded window now bounded by lease grace"
                ),
                Err(error) => tracing::warn!(
                    proj_id = %projection_id,
                    ?error,
                    "portal: failed to orphan projection lease on ungraceful drop"
                ),
            }
        }

        // 2. REAP + RECONCILE: any due projection lease (or a terminal lease
        // already reaped by the compositor) identifies exactly one projection.
        // This includes the long resident TTL: once it elapses, authority and
        // drive state expire with the scene surface instead of retaining a stale
        // cached lease/tile and scheduling its past deadline forever.
        let mut reaped = Vec::new();
        for (projection_id, lease_id, _) in &lifecycle {
            let already_terminal = scene
                .leases
                .get(lease_id)
                .is_some_and(|lease| lease.state.is_terminal());
            if already_terminal || scene.expire_lease(lease_id).is_some() {
                reaped.push(projection_id.clone());
            }
        }
        for proj_id in reaped {
            self.authority.expire_projection(&proj_id);
            if self.effective_transport(&proj_id) == PortalTransport::ResidentGrpcBridge {
                if let Some(tx) = &self.resident_grpc_bridge_tx {
                    let _ = tx.try_send(BridgeMessage::Detach {
                        projection_id: proj_id.clone(),
                    });
                }
            }
            self.drive.forget(&proj_id);
            tracing::info!(
                proj_id = %proj_id,
                "portal: surface removed on lease-grace expiry — projection expired, no further state"
            );
        }
    }

    fn drain_pending_tile_removals(&mut self, scene: &mut SceneGraph) {
        for tile_id in self.drive.drain_pending_tile_removals() {
            match scene.delete_tile(tile_id, PORTAL_DRIVER_NAMESPACE) {
                Ok(()) => {
                    tracing::info!(
                        tile_id = ?tile_id,
                        "portal drain: removed detached projection tile"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        tile_id = ?tile_id,
                        error = ?error,
                        "portal drain: failed to remove detached projection tile"
                    );
                }
            }
        }
        for lease_id in self.drive.drain_pending_lease_revocations() {
            match scene.revoke_lease(lease_id) {
                Ok(()) => tracing::info!(
                    lease_id = ?lease_id,
                    "portal drain: revoked detached projection lease"
                ),
                Err(error) => tracing::warn!(
                    lease_id = ?lease_id,
                    ?error,
                    "portal drain: failed to revoke detached projection lease"
                ),
            }
        }
    }
}

impl Default for InProcessPortalDriver {
    fn default() -> Self {
        Self::new()
    }
}

/// Get current wall-clock timestamp in microseconds since UNIX epoch.
fn now_wall_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u128::from(u64::MAX)) as u64
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_input::{DraftNotificationBatch, DraftSubmission, InputProcessor};
    use tze_hud_projection::{
        AcknowledgeInputRequest, AttachRequest, ContentClassification, GetPendingInputRequest,
        InputAckState, OperationEnvelope, OutputKind, PORTAL_UPDATE_RATE_WINDOW_WALL_US,
        PortalInputFeedbackState, PortalInputSubmission, ProjectionBounds, ProjectionOperation,
        ProviderKind, PublishOutputRequest,
    };
    use tze_hud_scene::{NodeData, SceneGraph};

    /// The header marker line the resident adapter paints while the agent is
    /// actively appending (mirrors `resident_grpc::PORTAL_ACTIVITY_MARKER_LINE`,
    /// which is private to that module). Used to assert the cue's presence /
    /// quiescence in the painted tile markdown.
    const PORTAL_ACTIVITY_MARKER_TEXT: &str = "⋯ writing";

    fn test_envelope(
        operation: ProjectionOperation,
        projection_id: &str,
        request_id: &str,
    ) -> OperationEnvelope {
        OperationEnvelope {
            operation,
            projection_id: projection_id.to_string(),
            request_id: request_id.to_string(),
            client_timestamp_wall_us: 1,
        }
    }

    fn attach_and_get_token_at(
        driver: &mut InProcessPortalDriver,
        projection_id: &str,
        server_timestamp_wall_us: u64,
    ) -> String {
        let resp = driver.authority_mut().handle_attach(
            AttachRequest {
                envelope: test_envelope(
                    ProjectionOperation::Attach,
                    projection_id,
                    &format!("attach-{projection_id}"),
                ),
                provider_kind: ProviderKind::Claude,
                display_name: format!("Test {projection_id}"),
                workspace_hint: None,
                repository_hint: None,
                icon_profile_hint: None,
                content_classification: ContentClassification::Private,
                hud_target: None,
                idempotency_key: None,
            },
            "test-caller",
            server_timestamp_wall_us,
        );
        assert!(resp.accepted, "attach must be accepted");
        resp.owner_token
            .expect("owner_token must be present after attach")
    }

    fn attach_and_get_token(driver: &mut InProcessPortalDriver, projection_id: &str) -> String {
        attach_and_get_token_at(driver, projection_id, 1000)
    }

    fn publish(
        driver: &mut InProcessPortalDriver,
        projection_id: &str,
        owner_token: &str,
        text: &str,
        ts: u64,
    ) {
        let resp = driver.authority_mut().handle_publish_output(
            PublishOutputRequest {
                envelope: test_envelope(
                    ProjectionOperation::PublishOutput,
                    projection_id,
                    &format!("pub-{projection_id}-{ts}"),
                ),
                owner_token: owner_token.to_string(),
                output_text: text.to_string(),
                output_kind: OutputKind::Assistant,
                content_classification: ContentClassification::Private,
                logical_unit_id: Some(format!("unit-{ts}")),
                coalesce_key: None,
                expects_reply: false,
            },
            "test-caller",
            ts,
        );
        assert!(resp.accepted, "publish_output must be accepted");
    }

    #[test]
    fn portal_composer_submission_enters_pending_input_queue_and_can_be_acknowledged() {
        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let projection_id = "proj-composer-return";
        let token = attach_and_get_token(&mut driver, projection_id);
        driver.attach_projection(projection_id, Vec::new());

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        publish(
            &mut driver,
            projection_id,
            &token,
            "assistant is ready",
            100,
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);
        let tile_id = driver
            .drive
            .entries
            .get(projection_id)
            .expect("drive entry must exist")
            .tile_scene_id
            .expect("drain must create a portal tile");

        let mut batch = DraftNotificationBatch::new();
        batch.record_submission(DraftSubmission {
            text: "please summarize the current diff".to_string(),
            sequence: 7,
        });

        let feedback = driver
            .submit_composer_batch_for_tile(
                tile_id,
                &batch,
                1_000,
                Some(2_000),
                ContentClassification::Private,
            )
            .expect("focused portal tile must map to an attached projection");
        assert_eq!(
            feedback.pending_input_count, 1,
            "submission must create one semantic pending-input item"
        );

        let poll = driver.authority_mut().handle_get_pending_input(
            GetPendingInputRequest {
                envelope: test_envelope(
                    ProjectionOperation::GetPendingInput,
                    projection_id,
                    "poll-composer-input",
                ),
                owner_token: token.clone(),
                max_items: Some(1),
                max_bytes: Some(4_096),
            },
            "codex-session",
            1_100,
        );
        assert!(poll.accepted, "pending-input poll must be accepted");
        assert_eq!(poll.pending_input.len(), 1);
        assert_eq!(
            poll.pending_input[0].submission_text,
            "please summarize the current diff"
        );

        let input_id = poll.pending_input[0].input_id.clone();
        let ack = driver.authority_mut().handle_acknowledge_input(
            AcknowledgeInputRequest {
                envelope: test_envelope(
                    ProjectionOperation::AcknowledgeInput,
                    projection_id,
                    "ack-composer-input",
                ),
                owner_token: token,
                input_id,
                ack_state: InputAckState::Handled,
                ack_message: None,
                not_before_wall_us: None,
            },
            "codex-session",
            1_200,
        );
        assert!(ack.accepted, "handled acknowledgement must be accepted");

        let state = driver
            .authority_mut()
            .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
            .expect("projection state must still exist");
        assert_eq!(
            state.pending_input_count,
            Some(0),
            "handled input must no longer count as waiting for the agent"
        );
    }

    /// Regression guard for the `notify_tile_content_appended` wiring at the
    /// RenderPortal drain site (portal_projection_driver.rs, spec §3.2).
    ///
    /// After a RenderPortal drain with content that overflows the viewport the
    /// `InputProcessor` must have advanced follow-tail and recorded the tile as
    /// at-tail in the scene graph.  This assertion only passes when the driver
    /// actually calls `notify_tile_content_appended`; removing that call causes
    /// `tile_follow_tail_at_tail` to remain `false` and the test to fail.
    ///
    /// Uses a deterministic `now_us` via `drain_inner` so the rate-window
    /// comparison is clock-independent.
    #[test]
    fn drain_render_portal_notify_tile_content_appended_wiring() {
        use tze_hud_projection::AdapterGeometrySnapshot;
        use tze_hud_projection::AdapterPortalRect;

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let token = attach_and_get_token(&mut driver, "proj-a");
        driver.attach_projection("proj-a", Vec::new());

        // Derive line height from the adapter's font token so the geometry is
        // consistent with whatever font size the design tokens resolve to.
        let font_size_px = driver
            .drive
            .entries
            .get("proj-a")
            .unwrap()
            .adapter
            .visual_tokens()
            .transcript_font_size_px;
        let line_h = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER;
        // One-line viewport: content with 10 units overflows and forces scroll.
        let viewport_h = (1.0 * line_h).ceil();

        // Push a geometry snapshot so the RenderPortal path uses the small
        // viewport — otherwise it falls back to the adapter configured height
        // which may be large enough that content never overflows.
        driver.authority_mut().push_geometry_snapshot(
            "proj-a",
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: viewport_h as i32,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Drain 1 (CreatePortalTile): publish at t=100 µs, drain at t=200 µs.
        publish(&mut driver, "proj-a", &token, "line-0", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let tile_id = driver
            .drive
            .entries
            .get("proj-a")
            .expect("drive entry must exist")
            .tile_scene_id
            .expect("tile must be created after first drain");

        // Confirm scroll config was registered.
        assert!(
            scene.tile_scroll_config(tile_id).is_some(),
            "tile must have scroll config after CreatePortalTile drain"
        );

        // Resize the tile to the small viewport so overflow is detectable.
        let _ = scene.update_tile_bounds(
            tile_id,
            Rect::new(0.0, 0.0, 600.0, viewport_h),
            PORTAL_DRIVER_NAMESPACE,
        );

        // Drain 2 (RenderPortal): publish 9 more lines at timestamps inside the
        // rate window, then drain at a time well past PORTAL_UPDATE_RATE_WINDOW_WALL_US
        // so the coalescer releases the update.
        let base_ts = PORTAL_UPDATE_RATE_WINDOW_WALL_US;
        for i in 1..=9_u64 {
            publish(
                &mut driver,
                "proj-a",
                &token,
                &format!("line-{i}"),
                base_ts + i * 5,
            );
        }
        // now_us is past the rate window — the authority will release the update.
        let drain2_now_us = base_ts + PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), drain2_now_us);

        // Primary regression assertion: notify_tile_content_appended must have been
        // called — it is the only code path that sets tile_follow_tail_at_tail.
        // If the call at the RenderPortal drain site is removed, this assertion fails.
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "spec §3.2 regression: tile must be at-tail after RenderPortal drain \
             with overflowing content — removing notify_tile_content_appended wiring \
             at the drain site causes this assertion to fail"
        );

        // Secondary: the tile must still exist (no panic / state reset).
        assert!(
            driver
                .drive
                .entries
                .get("proj-a")
                .unwrap()
                .tile_scene_id
                .is_some(),
            "tile must persist after RenderPortal drain"
        );

        // hud-g1ena.3: the RenderPortal drain must also plumb the ambient
        // unread-output count onto the tile so the compositor can render the
        // jump-to-latest badge. The driver writes the entry at the drain site, so
        // its presence proves the `set_tile_unread_count` wiring runs; the value
        // must reflect the just-drained batch's unread count (carried past the
        // drain-zero, matching the ambient indicator, hud-meqet), which this
        // fixture drives above zero by coalescing 9 appended lines. The badge's
        // scrolled-away visibility gate is covered by the compositor test.
        assert!(
            scene.overlay.tile_unread_counts.contains_key(&tile_id),
            "hud-g1ena.3 regression: RenderPortal drain must call set_tile_unread_count \
             so the jump-to-latest pill can carry the ambient unread count"
        );
        assert!(
            scene.tile_unread_count(tile_id) > 0,
            "the plumbed count must reflect the drained batch's unread output \
             (got {}), not a hardcoded zero",
            scene.tile_unread_count(tile_id)
        );
    }

    /// Regression guard (codex P2, hud-y4pzu PR #1131 review): the RenderPortal
    /// drain's append-geometry `total_lines` count must include `input_history`
    /// units, not just `update.visible_transcript`. `portal_markdown_with` paints
    /// the INPUT band into the SAME single-node markdown as the OUTPUT transcript
    /// whenever the portal is Expanded and `input_history` is non-empty — so a
    /// height count that only sees `visible_transcript` undercounts
    /// `new_content_height_px`, and long INPUT history cannot be scrolled to via
    /// the follow-tail clamp.
    ///
    /// Drives the discrepancy directly: the OUTPUT transcript grows by exactly
    /// one short line between the two drains, but ten viewer replies land in
    /// `input_history` in between. If the height count only sees the OUTPUT
    /// delta, the follow-tail clamp advances by about one line; counting the
    /// INPUT band too advances it by well over five lines.
    #[test]
    fn drain_render_portal_height_accounting_includes_input_history() {
        use tze_hud_input::DraftNotificationBatch as ComposerBatch;
        use tze_hud_projection::AdapterGeometrySnapshot;
        use tze_hud_projection::AdapterPortalRect;

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let token = attach_and_get_token(&mut driver, "proj-input-height");
        driver.attach_projection("proj-input-height", Vec::new());

        let font_size_px = driver
            .drive
            .entries
            .get("proj-input-height")
            .unwrap()
            .adapter
            .visual_tokens()
            .transcript_font_size_px;
        let line_h = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER;
        let viewport_h = (1.0 * line_h).ceil();

        driver.authority_mut().push_geometry_snapshot(
            "proj-input-height",
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: viewport_h as i32,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Drain 1 (CreatePortalTile).
        publish(&mut driver, "proj-input-height", &token, "line-0", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let tile_id = driver
            .drive
            .entries
            .get("proj-input-height")
            .expect("drive entry must exist")
            .tile_scene_id
            .expect("tile must be created after first drain");

        let _ = scene.update_tile_bounds(
            tile_id,
            Rect::new(0.0, 0.0, 600.0, viewport_h),
            PORTAL_DRIVER_NAMESPACE,
        );

        // Ten viewer replies land in `input_history` between the two drains.
        // `submit_portal_input` flags `portal_update_pending` but never registers
        // a coalescer entry by itself (only a PublishOutput append does that), so
        // this alone does not make the projection due — it rides in on the next
        // OUTPUT append below, exactly as production composer submissions do.
        for i in 0..10_u64 {
            let mut batch = ComposerBatch::new();
            batch.record_submission(tze_hud_input::DraftSubmission {
                text: format!("viewer reply {i}"),
                sequence: i,
            });
            let feedback = driver
                .submit_composer_batch_for_tile(
                    tile_id,
                    &batch,
                    300 + i,
                    None,
                    ContentClassification::Private,
                )
                .expect("focused tile must map to the attached projection");
            assert_eq!(feedback.feedback_state, PortalInputFeedbackState::Accepted);
        }

        // Drain 2 (RenderPortal): one more short OUTPUT line makes the projection
        // due again. The OUTPUT delta alone is exactly one line.
        let base_ts = PORTAL_UPDATE_RATE_WINDOW_WALL_US;
        publish(
            &mut driver,
            "proj-input-height",
            &token,
            "line-1",
            base_ts + 1,
        );
        let drain2_now_us = base_ts + PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), drain2_now_us);

        let (_, offset_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            offset_y > 5.0 * line_h,
            "follow-tail offset {offset_y} (line height {line_h}) must reflect the \
             ten-reply INPUT band, not just the one-line OUTPUT delta — the \
             RenderPortal drain's height accounting is not counting \
             `input_history`, so long INPUT history cannot be scrolled to"
        );
    }

    /// Follow-tail (spec §3.2): after a RenderPortal drain on an at-tail tile,
    /// the scroll offset advances and `tile_follow_tail_at_tail` is still true.
    ///
    /// Uses deterministic time injection via `drain_inner` (injectable clock seam
    /// introduced by hud-28j7v).
    #[test]
    fn follow_tail_advances_on_append_at_tail_tile() {
        use tze_hud_projection::AdapterGeometrySnapshot;
        use tze_hud_projection::AdapterPortalRect;

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let token = attach_and_get_token(&mut driver, "proj-b");
        driver.attach_projection("proj-b", Vec::new());

        // Derive line height from the adapter's font token.
        let font_size_px = driver
            .drive
            .entries
            .get("proj-b")
            .unwrap()
            .adapter
            .visual_tokens()
            .transcript_font_size_px;
        let line_h = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER;
        let viewport_h = (1.0 * line_h).ceil();

        // Push a geometry snapshot so the drain uses the small viewport.
        driver.authority_mut().push_geometry_snapshot(
            "proj-b",
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: viewport_h as i32,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Drain 1 (CreatePortalTile): deterministic t=200 µs.
        publish(&mut driver, "proj-b", &token, "unit-0", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let tile_id = driver
            .drive
            .entries
            .get("proj-b")
            .unwrap()
            .tile_scene_id
            .expect("tile must be created");

        // Resize tile to the 1-line viewport used by the geometry snapshot.
        let _ = scene.update_tile_bounds(
            tile_id,
            Rect::new(0.0, 0.0, 600.0, viewport_h),
            PORTAL_DRIVER_NAMESPACE,
        );

        // The tile's scroll config was registered during CreatePortalTile drain.
        // Confirm it's present before draining the RenderPortal step.
        assert!(
            scene.tile_scroll_config(tile_id).is_some(),
            "tile must have scroll config before RenderPortal drain"
        );

        // Publish 9 more units with timestamps inside the first rate window so the
        // coalescer accumulates them, then drain at a time past the window.
        let base_ts = PORTAL_UPDATE_RATE_WINDOW_WALL_US;
        for i in 1..=9_u64 {
            publish(
                &mut driver,
                "proj-b",
                &token,
                &format!("unit-{i}"),
                base_ts + i * 10 + 5,
            );
        }

        // Drain 2 (RenderPortal): now_us is past two rate windows so the update
        // is guaranteed due.  Deterministic timestamp — no wall-clock dependency.
        let drain2_now_us = base_ts + PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), drain2_now_us);

        // The driver must have called notify_tile_content_appended: with 10 lines
        // in a 1-line viewport the content overflows and at-tail stays true.
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "spec §3.2: tile must be at-tail after RenderPortal drain with overflow"
        );

        // The scroll offset must be positive: follow-tail advanced to show the tail.
        let (_, scroll_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            scroll_y > 0.0,
            "spec §3.2: scroll offset must advance for at-tail tile after overflow \
             (got scroll_y={scroll_y})"
        );

        // Drive entry must survive (no panic / state reset).
        assert!(
            driver
                .drive
                .entries
                .get("proj-b")
                .unwrap()
                .tile_scene_id
                .is_some(),
            "tile must persist after second drain"
        );
    }

    /// spec §3.3 — `notify_tile_content_appended` on a scrolled-back tile must
    /// NOT change the scroll offset (InputProcessor enforces this; driver just calls it).
    #[test]
    fn scrolled_back_tile_offset_is_stable_on_append() {
        use tze_hud_input::ScrollEvent;
        use tze_hud_scene::TileScrollConfig;

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("portal-agent", 60_000);
        let viewport_h = 200.0_f32;
        let tile_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(0.0, 0.0, 600.0, viewport_h),
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(
                tile_id,
                TileScrollConfig {
                    scrollable_x: false,
                    scrollable_y: true,
                    content_width: None,
                    content_height: None,
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();

        // Prime at-tail with large content.
        let line_h = 18.2_f32;
        let large_content = viewport_h * 5.0;
        processor.notify_tile_content_appended(
            tile_id,
            large_content,
            viewport_h,
            line_h,
            &mut scene,
        );

        // User scrolls back.  The tile occupies (0,0)→(600,200); use (300,100)
        // as the event coordinate so hit-testing resolves to this tile.
        let scroll_ev = ScrollEvent {
            x: 300.0,
            y: 100.0,
            delta_x: 0.0,
            delta_y: -50.0,
        };
        processor.process_scroll_event(&scroll_ev, &mut scene);

        let (_, pre_y) = scene.tile_scroll_offset_local(tile_id);

        // spec §3.3: another append must NOT change the scrolled-back position.
        let changed = processor.notify_tile_content_appended(
            tile_id,
            large_content + line_h,
            viewport_h,
            line_h,
            &mut scene,
        );

        let (_, post_y) = scene.tile_scroll_offset_local(tile_id);

        assert!(
            !changed,
            "spec §3.3: scrolled-back tile must not advance after append"
        );
        assert!(
            (post_y - pre_y).abs() < f32::EPSILON,
            "spec §3.3: scroll offset must be unchanged; pre={pre_y} post={post_y}"
        );
    }

    /// Regression guard for the §6b.4 geometry-snapshot producer wiring (hud-npq6g).
    ///
    /// `push_geometry_snapshot_for_tile` must forward a resize geometry snapshot
    /// into the `ProjectionAuthority` so that the drain loop consumer (the
    /// `geometry_batch` field on `ProjectedPortalState`) reads it on the next
    /// drain cycle.
    ///
    /// Precondition: `push_geometry_snapshot_for_tile` had zero production callers
    /// before hud-npq6g.  This test fails on the pre-wiring code where the only
    /// caller was a bin test (the authority snapshot therefore stays `None` in
    /// production, and the drain always falls back to the adapter-configured height).
    ///
    /// The test drives the full producer → consumer path:
    ///   1. Attach a projection session and drain to create the portal tile in
    ///      the scene.
    ///   2. Call `push_geometry_snapshot_for_tile` with a tiny (1-px) viewport
    ///      height — this is what the window management layer now does after a
    ///      hotkey resize.
    ///   3. Drain again with overflowing content and confirm that:
    ///      (a) the drain used the geometry-batch viewport — scroll advanced and
    ///      `tile_follow_tail_at_tail` is true, and
    ///      (b) the authority's pending batch was consumed so it is `None`
    ///      after the drain (no stale re-delivery).
    ///
    /// Removing the `push_geometry_snapshot_for_tile` call (or the wiring in
    /// windowed.rs) causes the drain to fall back to the adapter-configured height
    /// (typically 360 px).  With a 10-line transcript the content height is much
    /// less than 360 px, so `tile_follow_tail_at_tail` stays `false` and the
    /// first assertion fails.
    #[test]
    fn hotkey_resize_geometry_reaches_drain_consumer() {
        use tze_hud_input::{GeometrySnapshot, PortalRect};
        use tze_hud_projection::ProjectedPortalPolicy;

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let token = attach_and_get_token(&mut driver, "proj-resize");
        driver.attach_projection("proj-resize", Vec::new());

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Drain 1 (CreatePortalTile): publish one unit, drain to create the tile.
        publish(&mut driver, "proj-resize", &token, "initial-line", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let tile_id = driver
            .drive
            .entries
            .get("proj-resize")
            .expect("drive entry must exist")
            .tile_scene_id
            .expect("tile must be created after first drain");

        // Resize the scene tile to a tiny height so that overflow is detectable.
        // This simulates the hotkey-resize scene mutation in windowed.rs.
        let tiny_h = 1.0_f32;
        let _ = scene.update_tile_bounds(
            tile_id,
            tze_hud_scene::Rect::new(0.0, 0.0, 600.0, tiny_h),
            PORTAL_DRIVER_NAMESPACE,
        );

        // Producer wiring (hud-npq6g): push the geometry snapshot that the hotkey
        // resize path now emits. The sequence counter starts at 1.
        let geo_snapshot = GeometrySnapshot {
            portal_id_hash: 0,
            rect: PortalRect {
                x: 0.0,
                y: 0.0,
                width: 600.0,
                height: tiny_h,
            },
            gesture_active: false,
            sequence: 1,
        };
        let pushed = driver.push_geometry_snapshot_for_tile(tile_id, geo_snapshot, None);
        assert!(
            pushed,
            "push_geometry_snapshot_for_tile must return true for a known tile \
             — the producer wiring is broken if this fails"
        );

        // Verify the authority now holds the pending batch before the drain consumes it.
        {
            let state = driver
                .authority
                .projected_portal_state("proj-resize", &ProjectedPortalPolicy::permit_all())
                .expect("session must exist");
            let batch = state.geometry_batch.expect(
                "geometry_batch must be Some after push_geometry_snapshot_for_tile \
                 — the consumer path is broken if this fails",
            );
            assert_eq!(
                batch.latest.map(|s| s.sequence),
                Some(1),
                "pending batch must carry sequence 1 before drain"
            );
        }

        // Drain 2 (RenderPortal): publish overflowing content, drain past the rate window.
        let base_ts = PORTAL_UPDATE_RATE_WINDOW_WALL_US;
        for i in 1..=9_u64 {
            publish(
                &mut driver,
                "proj-resize",
                &token,
                &format!("overflow-line-{i}"),
                base_ts + i * 5,
            );
        }
        let drain2_now_us = base_ts + PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), drain2_now_us);

        // (a) The drain must have used the geometry-batch viewport (tiny_h = 1 px).
        // With a 1-px viewport and 10 lines of content the tile is deep in overflow,
        // so notify_tile_content_appended must have set the at-tail flag.
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "spec §6b.4 regression (hud-npq6g): drain must use the geometry-batch \
             viewport height from push_geometry_snapshot_for_tile. \
             Removing the push_geometry_snapshot_for_tile call (or wiring in windowed.rs) \
             causes the drain to fall back to the adapter-configured height (~360 px), \
             which is too large for 10 lines to overflow → tile_follow_tail_at_tail stays false."
        );

        // (b) The pending batch must be cleared after the drain (consume wiring).
        let state_after = driver
            .authority
            .projected_portal_state("proj-resize", &ProjectedPortalPolicy::permit_all())
            .expect("session must still exist");
        assert!(
            state_after.geometry_batch.is_none(),
            "geometry_batch must be None after the drain consumed it — \
             removing consume_geometry_batch from the drain causes stale re-delivery"
        );
    }

    /// hud-s62vv regression: a bridged (first-class-surface) projection's
    /// resize geometry must still reach `session.latest_geometry` even though it
    /// has no in-process tile.
    ///
    /// Live finding (R1, discovered from hud-8agm0's post-#1129 re-verify): on
    /// the resident-gRPC-bridged first-class-surface path, a whole-portal resize
    /// scales the outer tile/frame correctly (#1129), but the ADAPTER's next
    /// republish reverts the transcript wrap width and hit regions back to
    /// declared attach-time bounds — the opposite of what
    /// `adapter_republish_after_resize_keeps_transcript_wrapped_to_resized_pane`
    /// (in `tze_hud_runtime::windowed::portal`) asserts for the in-process path.
    ///
    /// Root cause: `push_geometry_snapshot_for_tile`'s reverse-lookup only ever
    /// matched `entry.tile_scene_id` — populated exclusively by the in-process
    /// tile-creation path (`drain_inner`'s `CreatePortalTile` arm). A bridged
    /// entry's `tile_scene_id` stays `None` for its whole lifetime (materialised
    /// solely over the bridge; see `bridged_projection_materialises_via_bridge_
    /// and_suppresses_direct_path`), so every resize on a bridged portal's real
    /// (wire-created) tile was silently dropped: `ProjectionAuthority::
    /// push_geometry_snapshot` was NEVER called, `session.latest_geometry` stayed
    /// `None` forever, and `ProjectedPortalState::resized_bounds` (which
    /// `ResidentGrpcPortalAdapter::bounds_for_state` / `local_bounds_for_state`
    /// read on every republish) fell back to the adapter's static declared
    /// `expanded_bounds`/`compact_bounds` config on every single render.
    ///
    /// This reproduces the LIVE flow ordering: attach a projection, route it to
    /// `PortalTransport::ResidentGrpcBridge` (no in-process tile is ever
    /// created), then push a geometry snapshot for the tile id the wire session
    /// server actually created (as the windowed resize handlers now resolve via
    /// the resized tile's declared `PortalSurface::identity.session_id`, read
    /// from the scene while the caller already held the scene lock). Before the
    /// fix, `push_geometry_snapshot_for_tile` had no `bridged_portal_id_hint`
    /// parameter at all and the reverse-lookup unconditionally returned `false`
    /// for this tile; after the fix it decodes the hint back to the attached
    /// `projection_id` and the push succeeds.
    #[test]
    fn bridged_resize_geometry_reaches_latest_geometry_via_portal_surface_hint() {
        use tze_hud_input::{GeometrySnapshot, PortalRect};
        use tze_hud_projection::ProjectedPortalPolicy;
        use tze_hud_scene::SceneId;

        let mut driver = InProcessPortalDriver::new();

        let proj = "proj-bridged-resize";
        let _token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());
        driver.set_projection_transport(proj, PortalTransport::ResidentGrpcBridge);

        // A bridged projection never gets an in-process tile — confirm the
        // precondition this regression depends on (mirrors
        // `bridged_projection_materialises_via_bridge_and_suppresses_direct_path`).
        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .expect("drive entry must exist after attach")
                .tile_scene_id
                .is_none(),
            "precondition: a bridged projection must have no in-process tile"
        );

        // The tile id the wire session server actually created for this bridged
        // portal's transcript surface. The windowed runtime never learns this via
        // `entry.tile_scene_id` (it stays `None`) — only via the resized tile's
        // own scene-declared portal-surface identity.
        let wire_tile_id = SceneId::new();
        let geo_snapshot = GeometrySnapshot {
            portal_id_hash: 0,
            rect: PortalRect {
                x: 10.0,
                y: 20.0,
                width: 640.0,
                height: 400.0,
            },
            gesture_active: false,
            sequence: 1,
        };

        // Pre-fix behaviour: with no hint (or a stale/unrelated one), the tile is
        // unresolvable and the push is correctly rejected — a resize must never
        // be attributed to the wrong (or no) projection.
        assert!(
            !driver.push_geometry_snapshot_for_tile(wire_tile_id, geo_snapshot, None),
            "a tile with no matching in-process entry and no portal-surface hint \
             must not resolve to any projection"
        );

        // The fix: the resized tile's declared portal-surface identity (what the
        // adapter stamps as `PortalIdentity::session_id` via
        // `portal_id_for_projection`, RFC 0013 §7.2) decodes back to the attached
        // `projection_id` and the push now succeeds.
        let portal_id_hint = format!("text-stream://projection/{proj}");
        assert!(
            driver.push_geometry_snapshot_for_tile(
                wire_tile_id,
                geo_snapshot,
                Some(&portal_id_hint)
            ),
            "push_geometry_snapshot_for_tile must resolve a bridged tile via its \
             portal-surface identity hint — the producer wiring is broken (or was \
             never extended to the bridged transport) if this fails"
        );

        // Consumer-side proof: `session.latest_geometry` (durable, unlike the
        // transient `pending_geometry_batch`) now carries the resize, so
        // `ProjectedPortalState::resized_bounds` — the exact field
        // `ResidentGrpcPortalAdapter::bounds_for_state` /
        // `local_bounds_for_state` read on every republish — reflects the new
        // size instead of falling back to the adapter's static declared bounds.
        let state = driver
            .authority
            .projected_portal_state(proj, &ProjectedPortalPolicy::permit_all())
            .expect("session must exist");
        let resized = state.resized_bounds.expect(
            "resized_bounds must be Some after a bridged resize reaches \
             session.latest_geometry — a None here reproduces hud-s62vv: every \
             subsequent adapter republish would fall back to declared \
             attach-time config bounds for both the transcript wrap width and \
             every derived hit region (composer, resize band)",
        );
        assert_eq!(resized.width_px, 640);
        assert_eq!(resized.height_px, 400);
    }

    /// hud-ttq97: verify that a drained `RenderPortal` update with a known
    /// `submitted_at_us` records the expected publish-to-present delta into the
    /// `portal_publish_to_present_latency` bucket.
    ///
    /// Test shape:
    /// 1. Attach a projection session and drain (CreatePortalTile) at `t=200 µs`.
    ///    The bucket must have zero samples after the create drain (only `RenderPortal`
    ///    drains contribute samples).
    /// 2. Publish content with a known `submitted_at_us = 1_000 µs` and drain at
    ///    `now_us = 6_000 µs`.  Expected delta: `6_000 − 1_000 = 5_000 µs`.
    /// 3. Assert the bucket has exactly one sample with value `5_000 µs`.
    ///
    /// Removing the `self.portal_publish_to_present_latency.record(delta_us)` call
    /// at the `RenderPortal` drain site causes the bucket to remain empty and the
    /// final assertion to fail.
    #[test]
    fn render_portal_drain_records_publish_to_present_latency() {
        use tze_hud_projection::AdapterGeometrySnapshot;
        use tze_hud_projection::AdapterPortalRect;

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let token = attach_and_get_token(&mut driver, "proj-lat");
        driver.attach_projection("proj-lat", Vec::new());

        // Push a geometry snapshot so the drain has a valid viewport.
        driver.authority_mut().push_geometry_snapshot(
            "proj-lat",
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: 360,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Drain 1 (CreatePortalTile): publish at t=100 µs, drain at t=200 µs.
        // The bucket must be empty after a CreatePortalTile drain.
        publish(&mut driver, "proj-lat", &token, "initial-line", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        assert_eq!(
            driver.portal_publish_to_present_latency().samples.len(),
            0,
            "CreatePortalTile drain must not record a latency sample — only \
             RenderPortal drains contribute to the bucket"
        );

        // Drain 2 (RenderPortal): publish at submitted_at_us = 1_000 µs, drain
        // at now_us = 6_000 µs.  Expected delta: 6_000 − 1_000 = 5_000 µs.
        //
        // Use `base_ts` past the initial rate window so the coalescer releases the update.
        let submitted_at_us = PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1_000;
        let drain2_now_us = submitted_at_us + 5_000;
        publish(
            &mut driver,
            "proj-lat",
            &token,
            "content-line",
            submitted_at_us,
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), drain2_now_us);

        // Assert exactly one sample was recorded with the expected delta.
        let bucket = driver.portal_publish_to_present_latency();
        assert_eq!(
            bucket.samples.len(),
            1,
            "RenderPortal drain with submitted_at_us > 0 must record exactly one \
             latency sample; removing the record() call at the drain site causes \
             this to fail with 0 samples"
        );
        assert_eq!(
            bucket.samples[0], 5_000,
            "publish-to-present latency must be drain_now_us − submitted_at_us = \
             {drain2_now_us} − {submitted_at_us} = 5_000 µs"
        );
    }

    /// Regression guard for the `apply_token_map` → drive state propagation
    /// wiring (hud-be6ee acceptance criterion: token wiring reaches live adapters).
    ///
    /// The test verifies that:
    /// 1. A token map applied *before* an adapter attaches is inherited by the
    ///    new adapter's `PortalVisualTokens` at attach time.
    /// 2. A token map applied *after* an adapter attaches propagates immediately
    ///    to all live adapters' `PortalVisualTokens`.
    ///
    /// Removing the `self.drive.apply_token_map(overrides)` call in
    /// `InProcessPortalDriver::apply_token_map` causes the post-attach assertion
    /// to fail because `entry.adapter.visual_tokens()` still reflects the
    /// default (empty) token map rather than the overridden values.
    #[test]
    fn apply_token_map_propagates_to_drive_state() {
        let mut driver = InProcessPortalDriver::new();

        // Build a token map with a clearly non-default transcript font size.
        // The canonical key is `portal.transcript.font_size` (no `_px` suffix)
        // as declared by `PORTAL_TOKEN_TRANSCRIPT_FONT_SIZE` in portal_tokens.rs.
        let mut tokens = DesignTokenMap::new();
        tokens.insert("portal.transcript.font_size".to_string(), "32".to_string());

        // --- Part 1: pre-attach token map propagates to new adapters -------
        driver.apply_token_map(tokens.clone());

        // Attach *after* the token map is applied.
        attach_and_get_token(&mut driver, "proj-token-pre");
        driver.attach_projection("proj-token-pre", Vec::new());

        let font_size_pre = driver
            .drive
            .entries
            .get("proj-token-pre")
            .expect("drive entry must exist after attach")
            .adapter
            .visual_tokens()
            .transcript_font_size_px;

        // The driver's `token_overrides` map was set before attach, so
        // `InProcessPortalDriveState::attach` calls `resolve_visual_tokens()`
        // using those overrides. The adapter must reflect the exact custom value
        // rather than the default (13.0).  Using `assert_eq!` here ensures the
        // test fails if the wrong key is looked up or the map is not propagated.
        assert_eq!(
            font_size_pre, 32.0,
            "adapter font size must match the pre-attach overridden value (32.0)"
        );

        // --- Part 2: post-attach token map propagates to live adapters -----
        // Reset to empty map so we can observe the change clearly.
        driver.apply_token_map(DesignTokenMap::new());

        // Attach a second adapter before applying the custom map.
        attach_and_get_token(&mut driver, "proj-token-post");
        driver.attach_projection("proj-token-post", Vec::new());

        // Now apply a custom token map — must propagate to *both* live adapters.
        let mut tokens2 = DesignTokenMap::new();
        tokens2.insert("portal.transcript.font_size".to_string(), "48".to_string());
        driver.apply_token_map(tokens2.clone());

        // Removing the `set_visual_tokens` loop inside `apply_token_map`
        // causes the post-apply assertion to fail (value stays at the default,
        // 13.0, rather than the overridden 48.0).
        let font_size_post = driver
            .drive
            .entries
            .get("proj-token-post")
            .expect("second drive entry must exist")
            .adapter
            .visual_tokens()
            .transcript_font_size_px;

        assert_eq!(
            font_size_post, 48.0,
            "adapter font size must match the post-attach overridden value (48.0); \
             removing the set_visual_tokens loop in apply_token_map causes this to fail"
        );

        // Both entries must have been updated (not just the most recently attached).
        let font_size_pre_after = driver
            .drive
            .entries
            .get("proj-token-pre")
            .expect("first drive entry must still exist")
            .adapter
            .visual_tokens()
            .transcript_font_size_px;

        assert_eq!(
            font_size_pre_after, 48.0,
            "pre-existing adapter must also be updated to 48.0 by apply_token_map; \
             only the new-entry path in attach() being correct is not sufficient"
        );
    }

    /// hud-fm0nf: `apply_token_map` must ALSO forward the re-resolved tokens to
    /// the resident gRPC bridge (when one is installed) as a
    /// `BridgeMessage::SetVisualTokens`, so a bridged portal re-renders with the
    /// new active-profile tokens on hot-reload — parity with the in-process
    /// adapters updated in the same call. The forwarded palette must be resolved
    /// through the same `resolve_bridge_visual_tokens` helper ygtiy wired at
    /// spawn, so the override appears in the bridge's tokens.
    #[test]
    fn apply_token_map_forwards_resolved_tokens_to_bridge() {
        let mut driver = InProcessPortalDriver::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeMessage>(16);
        driver.set_resident_grpc_bridge_tx(Some(tx));

        // A non-default collapsed font size sentinel (canonical key
        // `portal.collapsed.font_size`, no `_px` suffix).
        let mut tokens = DesignTokenMap::new();
        tokens.insert(
            "portal.collapsed_card.font_size".to_string(),
            "42".to_string(),
        );
        driver.apply_token_map(tokens);

        // Exactly one SetVisualTokens must have been forwarded, carrying the
        // resolved sentinel — the same value the spawn-time helper would produce.
        let mut set_token_msgs = 0;
        let mut observed_font_size = None;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                BridgeMessage::SetVisualTokens(tokens) => {
                    set_token_msgs += 1;
                    observed_font_size = Some(tokens.collapsed_font_size_px);
                }
                other => panic!("unexpected bridge message on apply_token_map: {other:?}"),
            }
        }
        assert_eq!(
            set_token_msgs, 1,
            "apply_token_map must forward exactly one SetVisualTokens to the bridge"
        );
        assert_eq!(
            observed_font_size,
            Some(42.0),
            "the forwarded bridge tokens must carry the resolved active-profile \
             override (collapsed font size 42), not the spawn-time default"
        );
    }

    /// hud-fm0nf: with NO bridge installed, `apply_token_map` must not attempt to
    /// forward anything (the in-process-only path is unchanged).
    #[test]
    fn apply_token_map_without_bridge_does_not_forward() {
        let mut driver = InProcessPortalDriver::new();
        // No bridge tx installed. This must simply not panic and must still update
        // in-process adapters (covered by apply_token_map_propagates_to_drive_state).
        let mut tokens = DesignTokenMap::new();
        tokens.insert(
            "portal.collapsed_card.font_size".to_string(),
            "42".to_string(),
        );
        driver.apply_token_map(tokens);
        // Reaching here without panicking is the assertion: the None-bridge branch
        // is a no-op forward.
    }

    /// Regression guard for the `dispatch_portal_op` → `drain_inner` wiring
    /// introduced by hud-bq0gl.2.
    ///
    /// Exercises the full MCP channel path:
    ///
    /// 1. `dispatch_portal_op(PortalOp::Attach)` — must create a drive entry
    ///    and return an owner token through the one-shot reply channel.
    /// 2. `dispatch_portal_op(PortalOp::PublishOutput)` — must feed content into
    ///    the cadence coalescer.
    /// 3. `drain_inner` — must materialise the coalesced update into the scene and
    ///    call `notify_tile_content_appended` so `tile_follow_tail_at_tail` is
    ///    true (spec §3.2).
    ///
    /// If `dispatch_portal_op` is removed or the authority calls are deleted, the
    /// drive entry is never created, `drain_inner` finds nothing to do, and the
    /// `tile_follow_tail_at_tail` assertion fails.
    #[test]
    fn dispatch_portal_op_attach_publish_drain_wiring() {
        use tze_hud_projection::{AdapterGeometrySnapshot, AdapterPortalRect, ProjectionBounds};

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_dispatch_portal_op"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        // ── Step 1: Attach via dispatch_portal_op ──────────────────────────────
        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "test-proj".to_string(),
            display_name: "Test Projection".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });

        // The reply channel must carry the owner token — no async runtime needed
        // because dispatch_portal_op is synchronous.
        let owner_token = attach_rx
            .try_recv()
            .expect("reply must be sent synchronously")
            .expect("Attach must be accepted");
        assert!(
            !owner_token.is_empty(),
            "owner token must be non-empty on successful attach"
        );

        // Drive entry must exist after Attach.
        assert!(
            driver.drive.entries.contains_key("test-proj"),
            "dispatch_portal_op(Attach) must create a drive entry; \
             removing the attach_projection call causes this to fail"
        );

        // ── Step 2: PublishOutput via dispatch_portal_op ───────────────────────
        let (pub_tx, mut pub_rx) = tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: "test-proj".to_string(),
            owner_token: owner_token.clone(),
            output_text: "Hello from MCP portal".to_string(),
            logical_unit_id: None,
            output_kind: None,
            content_classification: None,
            coalesce_key: None,
            expects_reply: None,
            reply: pub_tx,
        });

        pub_rx
            .try_recv()
            .expect("publish reply must be sent synchronously")
            .expect("PublishOutput must be accepted");

        // ── Step 3: drain_inner materialises content into scene ────────────────
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Supply a geometry snapshot so the adapter has valid bounds.
        driver.authority_mut().push_geometry_snapshot(
            "test-proj",
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: 200,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        // First drain: CreatePortalTile.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let tile_id = driver
            .drive
            .entries
            .get("test-proj")
            .expect("drive entry must still exist after drain")
            .tile_scene_id
            .expect("drain must create a portal tile in the scene");

        assert!(
            scene.tile_scroll_config(tile_id).is_some(),
            "portal tile must have a scroll config after CreatePortalTile drain"
        );

        // Resize the tile to a 1-line viewport so overflow and follow-tail are
        // detectable.
        let font_size_px = driver
            .drive
            .entries
            .get("test-proj")
            .unwrap()
            .adapter
            .visual_tokens()
            .transcript_font_size_px;
        let line_h = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER;
        let viewport_h = (1.0 * line_h).ceil();
        let _ = scene.update_tile_bounds(
            tile_id,
            Rect::new(0.0, 0.0, 600.0, viewport_h),
            PORTAL_DRIVER_NAMESPACE,
        );

        // Publish 9 more lines, then drain past the rate window.
        for i in 1..=9_u64 {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            driver.dispatch_portal_op(PortalOp::PublishOutput {
                projection_id: "test-proj".to_string(),
                owner_token: owner_token.clone(),
                output_text: format!("line {i}"),
                logical_unit_id: Some(format!("u{i}")),
                output_kind: None,
                content_classification: None,
                coalesce_key: None,
                expects_reply: None,
                reply: tx,
            });
        }

        let drain2_now_us = PORTAL_UPDATE_RATE_WINDOW_WALL_US * 2 + 1;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), drain2_now_us);

        // Primary regression assertion (hud-bq0gl.2):
        // The MCP-channel wiring must result in notify_tile_content_appended
        // being called for overflowing content, setting tile_follow_tail_at_tail.
        // Removing dispatch_portal_op or the drain_inner call causes this to fail.
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "hud-bq0gl.2 regression: portal content published via dispatch_portal_op \
             must flow through drain_inner and set tile_follow_tail_at_tail (spec §3.2); \
             removing the dispatch_portal_op → authority wiring causes this to fail"
        );
    }

    #[test]
    fn dispatch_portal_op_list_maps_only_content_free_summaries() {
        let mut driver = InProcessPortalDriver::new();
        let (attach_tx, attach_rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "listed-projection".to_string(),
            display_name: "Listed projection".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let owner_token = attach_rx
            .blocking_recv()
            .expect("attach reply must arrive")
            .expect("attach must be accepted");

        let (publish_tx, publish_rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: "listed-projection".to_string(),
            owner_token,
            output_text: "private transcript must not be listed".to_string(),
            logical_unit_id: Some("listed-output".to_string()),
            output_kind: None,
            content_classification: None,
            coalesce_key: None,
            expects_reply: None,
            reply: publish_tx,
        });
        publish_rx
            .blocking_recv()
            .expect("publish reply must arrive")
            .expect("publish must be accepted");
        driver
            .authority_mut()
            .enqueue_input(
                "listed-projection",
                "listed-input",
                "private input must not be listed".to_string(),
                1,
                1_000,
                None,
            )
            .expect("test input must be accepted");

        let (list_tx, list_rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::List { reply: list_tx });
        let batch = list_rx
            .blocking_recv()
            .expect("list reply must arrive")
            .expect("list must be accepted");

        assert_eq!(batch.projections.len(), 1);
        assert_eq!(batch.projections[0].projection_id, "listed-projection");
        assert_eq!(batch.projections[0].unread_output_count, 1);
        assert_eq!(batch.projections[0].pending_input_count, 1);
        let payload = serde_json::to_string(&batch).expect("list batch must serialize");
        assert!(
            !payload.contains("private transcript must not be listed")
                && !payload.contains("private input must not be listed")
                && !payload.contains("owner_token"),
            "driver list mapping must never forward portal content or owner credentials: {payload}"
        );
    }

    /// Regression guard (hud-utbiy): the cooperative in-process drain must paint
    /// the published transcript onto the tile, not just create an empty tile with
    /// a scroll config.
    ///
    /// Before the fix, `drain_inner` created the portal tile and tracked scroll
    /// geometry but never rendered the transcript content — the coalesced update
    /// (which carries the text) was consumed by the create arm and discarded,
    /// leaving a permanent empty grey tile. This test asserts the tile's root
    /// node is a `TextMarkdown` whose content includes the published output text.
    ///
    /// It fails before the fix (the tile has no root node) and passes after.
    #[test]
    fn drain_paints_published_transcript_onto_tile() {
        use tze_hud_projection::{AdapterGeometrySnapshot, AdapterPortalRect, ProjectionBounds};

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_paint_content"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        // Attach.
        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "paint-proj".to_string(),
            display_name: "Paint Projection".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let owner_token = attach_rx
            .try_recv()
            .expect("reply must be sent synchronously")
            .expect("Attach must be accepted");

        // Publish a distinctive transcript line.
        const PUBLISHED: &str = "UNIQUE-TRANSCRIPT-MARKER-42";
        let (pub_tx, mut pub_rx) = tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: "paint-proj".to_string(),
            owner_token: owner_token.clone(),
            output_text: PUBLISHED.to_string(),
            logical_unit_id: None,
            output_kind: None,
            content_classification: None,
            coalesce_key: None,
            expects_reply: None,
            reply: pub_tx,
        });
        pub_rx
            .try_recv()
            .expect("publish reply must be sent synchronously")
            .expect("PublishOutput must be accepted");

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.authority_mut().push_geometry_snapshot(
            "paint-proj",
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: 200,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        // First drain: creates the tile AND paints the first publish's content.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let tile_id = driver
            .drive
            .entries
            .get("paint-proj")
            .expect("drive entry must still exist after drain")
            .tile_scene_id
            .expect("drain must create a portal tile in the scene");

        let assert_content_present = |scene: &SceneGraph, ctx: &str| {
            scene
                .tiles
                .get(&tile_id)
                .expect("portal tile must exist in scene")
                .root_node
                .unwrap_or_else(|| {
                    panic!(
                        "{ctx}: portal tile has no root node — content was never painted \
                         (the cooperative grey-tile bug)"
                    )
                });
            let content = tile_markdown(scene, tile_id);
            assert!(
                content.contains(PUBLISHED),
                "{ctx}: portal subtree must contain the published text \
                 {PUBLISHED:?}; got content: {content:?}"
            );
        };

        // First-publish (create-arm) content must be painted.
        assert_content_present(&scene, "after create drain");

        // A subsequent publish (RenderPortal arm) must also repaint content.
        const PUBLISHED_2: &str = "SECOND-TRANSCRIPT-MARKER-99";
        let (pub_tx2, _rx2) = tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: "paint-proj".to_string(),
            owner_token,
            output_text: PUBLISHED_2.to_string(),
            logical_unit_id: Some("u2".to_string()),
            output_kind: None,
            content_classification: None,
            coalesce_key: None,
            expects_reply: None,
            reply: pub_tx2,
        });
        let drain2_now_us = PORTAL_UPDATE_RATE_WINDOW_WALL_US * 2 + 1;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), drain2_now_us);

        // The retained transcript window keeps prior lines, so both markers must
        // be present after the second (RenderPortal) drain.
        scene
            .tiles
            .get(&tile_id)
            .unwrap()
            .root_node
            .expect("after render drain: portal tile must still have a painted root node");
        let content = tile_markdown(&scene, tile_id);
        assert!(
            content.contains(PUBLISHED_2),
            "after render drain: portal subtree must contain the second \
             published text {PUBLISHED_2:?}; got content: {content:?}"
        );
    }

    /// Migration guard (hud-rpm9s): the in-process cooperative driver declares
    /// the FIRST-CLASS portal surface onto the tile (not only raw tiles). After
    /// the create drain, `scene.portal_surface(tile_id)` must resolve to the
    /// governed descriptor — proving the driver applied the `SetPortalSurface`
    /// declaration (and `UpdatePortalSurfaceState` patch) through
    /// `apply_portal_render_batch_to_scene`'s new arms, in parity with the wire
    /// path. The raw-tile content root must ALSO still be painted (escape hatch).
    #[test]
    fn drain_declares_first_class_portal_surface_on_tile() {
        use tze_hud_projection::{AdapterGeometrySnapshot, AdapterPortalRect, ProjectionBounds};

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_surface_decl"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "surface-proj".to_string(),
            display_name: "Surface Projection".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let owner_token = attach_rx
            .try_recv()
            .expect("reply must be sent synchronously")
            .expect("Attach must be accepted");

        let (pub_tx, mut pub_rx) = tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: "surface-proj".to_string(),
            owner_token,
            output_text: "hello surface".to_string(),
            logical_unit_id: None,
            output_kind: None,
            content_classification: None,
            coalesce_key: None,
            expects_reply: None,
            reply: pub_tx,
        });
        pub_rx
            .try_recv()
            .expect("publish reply must be sent synchronously")
            .expect("PublishOutput must be accepted");

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.authority_mut().push_geometry_snapshot(
            "surface-proj",
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: 200,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let tile_id = driver
            .drive
            .entries
            .get("surface-proj")
            .expect("drive entry must exist")
            .tile_scene_id
            .expect("drain must create a portal tile");

        // The first-class surface descriptor must be declared on the tile.
        let surface = scene
            .portal_surface(tile_id)
            .expect("drain must declare a first-class portal surface on the tile");
        assert!(
            surface
                .parts
                .iter()
                .any(|p| p.kind == tze_hud_scene::types::PortalPartKind::Frame),
            "declared surface must carry the Frame part"
        );
        for expected in [
            tze_hud_scene::types::PortalPartKind::Header,
            tze_hud_scene::types::PortalPartKind::Composer,
            tze_hud_scene::types::PortalPartKind::Transcript,
            tze_hud_scene::types::PortalPartKind::Divider,
        ] {
            assert!(
                surface.parts.iter().any(|part| part.kind == expected),
                "native expanded surface must declare {expected:?}"
            );
        }
        assert_eq!(
            surface.display_state,
            tze_hud_scene::types::PortalDisplayState::Expanded,
            "an expanded portal must declare an Expanded display state"
        );

        // Escape hatch: the raw-tile content root must still be painted.
        assert!(
            scene
                .tiles
                .get(&tile_id)
                .expect("tile exists")
                .root_node
                .is_some(),
            "raw-tile content root must still be painted alongside the surface"
        );
        let root_id = scene.tiles.get(&tile_id).unwrap().root_node.unwrap();
        let root = scene.nodes.get(&root_id).expect("root node resolves");
        assert!(
            matches!(root.data, NodeData::SolidColor(_)),
            "production drain must materialize the native frame root"
        );
        let child_text: Vec<&str> = root
            .children
            .iter()
            .filter_map(|id| match &scene.nodes.get(id)?.data {
                NodeData::TextMarkdown(text) => Some(text.content.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            child_text.contains(&"INPUT"),
            "INPUT label must materialize"
        );
        assert!(
            child_text.contains(&"OUTPUT"),
            "OUTPUT label must materialize"
        );
        assert!(
            child_text.iter().any(|text| text.contains("hello surface")),
            "OUTPUT transcript must materialize in the native subtree"
        );
    }

    /// Unit guard for the redaction/zero semantics of the unread-count carry
    /// (hud-meqet). The driver drives `carry_drained_unread_count` on every
    /// drained state; this pins its three cases so redaction can never be
    /// leaked and a genuine zero can never be surfaced as a phantom indicator.
    #[test]
    fn carry_drained_unread_count_preserves_redaction_and_zero() {
        // Revealed slot (`Some`): refresh to the drained batch count. The
        // incoming `Some(0)` models the zeroed-session read that caused the bug.
        assert_eq!(carry_drained_unread_count(Some(0), 3), Some(3));
        assert_eq!(carry_drained_unread_count(Some(9), 4), Some(4));
        // Revealed but nothing drained: stays `Some(0)` — the downstream
        // indicator suppresses this exactly like a redacted `None`.
        assert_eq!(carry_drained_unread_count(Some(2), 0), Some(0));
        // Redacted slot (`None`): never resurrected into a leaked count, even
        // for a large drained batch.
        assert_eq!(carry_drained_unread_count(None, 7), None);
        assert_eq!(carry_drained_unread_count(None, 0), None);
    }

    /// Regression guard (hud-meqet): the ambient unread-output-count indicator
    /// must survive the REAL production drain-then-render ordering.
    ///
    /// `take_due_portal_update` zeroes `session.unread_output_count` as it
    /// consumes the drain, and the render state is built AFTER that from the
    /// (now-zeroed) session — so `projected_portal_state` alone reports
    /// `Some(0)` and the indicator suppresses on every real coalesced drain.
    /// The prior tests only ever built state WITHOUT a preceding drain, leaving
    /// this ordering structurally uncovered. Here we publish three outputs,
    /// run the actual `drain_inner` loop, and assert the painted tile carries
    /// the live `"3 unread"` line — while a direct `projected_portal_state`
    /// still reads the zeroed `Some(0)`, proving the count is sourced from the
    /// drained batch, not the session. Fails before the fix, passes after.
    #[test]
    fn drain_then_render_surfaces_live_unread_count() {
        use tze_hud_projection::{
            AdapterGeometrySnapshot, AdapterPortalRect, ProjectedPortalPolicy, ProjectionBounds,
        };

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_unread_indicator"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        // Attach.
        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "unread-proj".to_string(),
            display_name: "Unread Projection".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let owner_token = attach_rx
            .try_recv()
            .expect("reply must be sent synchronously")
            .expect("Attach must be accepted");

        // Publish three distinct (un-coalesced) outputs → unread_output_count = 3.
        for i in 0..3 {
            let (pub_tx, mut pub_rx) =
                tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
            driver.dispatch_portal_op(PortalOp::PublishOutput {
                projection_id: "unread-proj".to_string(),
                owner_token: owner_token.clone(),
                output_text: format!("line-{i}"),
                logical_unit_id: None,
                output_kind: None,
                content_classification: None,
                coalesce_key: None,
                expects_reply: None,
                reply: pub_tx,
            });
            pub_rx
                .try_recv()
                .expect("publish reply must be sent synchronously")
                .expect("PublishOutput must be accepted");
        }

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.authority_mut().push_geometry_snapshot(
            "unread-proj",
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: 200,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        // Drive the real drain: take_due (zeroes the session) THEN build state
        // THEN render into the tile.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        // The session is now zeroed, so a direct state read reports Some(0) —
        // this is exactly the stale read that suppressed the indicator.
        let post_drain_state = driver
            .authority_mut()
            .projected_portal_state("unread-proj", &ProjectedPortalPolicy::permit_all())
            .expect("projection must still exist after drain");
        assert_eq!(
            post_drain_state.unread_output_count,
            Some(0),
            "session unread must be zeroed post-drain — the count must be sourced \
             from the drained batch, not the session"
        );

        // The painted tile must nonetheless carry the live drained count.
        let tile_id = driver
            .drive
            .entries
            .get("unread-proj")
            .expect("drive entry must still exist after drain")
            .tile_scene_id
            .expect("drain must create a portal tile in the scene");
        let content = tile_markdown(&scene, tile_id);
        assert!(
            content.contains("3 unread"),
            "the ambient unread indicator must survive drain-then-render for a \
             nonzero count; expected a `3 unread` line, got content: {content:?}"
        );
    }

    /// Regression (hud-n95zc): the in-transcript unread divider reads
    /// `visible_unread_output_count`, not the ambient `unread_output_count`
    /// covered by the sibling test above — and that field had NO carry-forward
    /// fix, so `take_due_portal_update` zeroing `session.unread_output_count`
    /// before `projected_portal_state` derived it meant the divider never
    /// rendered on a real drain, even after hud-meqet fixed the ambient ticker.
    /// Existing divider unit tests in `resident_grpc.rs` all construct
    /// `ProjectedPortalState` fixtures directly and so never exercised this
    /// zero-then-read ordering; this drives the real `drain_inner`
    /// drain-then-render path (`take_due` zeroes the session, THEN state is
    /// built, THEN it is painted) and asserts the divider marker survives.
    #[test]
    fn drain_then_render_surfaces_unread_divider() {
        use tze_hud_projection::{AdapterGeometrySnapshot, AdapterPortalRect, ProjectionBounds};

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_unread_divider"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        // Attach.
        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "unread-divider-proj".to_string(),
            display_name: "Unread Divider Projection".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let owner_token = attach_rx
            .try_recv()
            .expect("reply must be sent synchronously")
            .expect("Attach must be accepted");

        // Publish two agent-authored (non-viewer) turns → unread_output_count = 2,
        // so there is a nonviewer retained unit for the divider to sit before.
        for i in 0..2 {
            let (pub_tx, mut pub_rx) =
                tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
            driver.dispatch_portal_op(PortalOp::PublishOutput {
                projection_id: "unread-divider-proj".to_string(),
                owner_token: owner_token.clone(),
                output_text: format!("turn-{i}"),
                logical_unit_id: None,
                output_kind: None,
                content_classification: None,
                coalesce_key: None,
                expects_reply: None,
                reply: pub_tx,
            });
            pub_rx
                .try_recv()
                .expect("publish reply must be sent synchronously")
                .expect("PublishOutput must be accepted");
        }

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.authority_mut().push_geometry_snapshot(
            "unread-divider-proj",
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: 200,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        // Drive the real drain: take_due (zeroes the session) THEN build state
        // THEN render into the tile — the exact production ordering that
        // starved `visible_unread_output_count` before this fix.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let tile_id = driver
            .drive
            .entries
            .get("unread-divider-proj")
            .expect("drive entry must still exist after drain")
            .tile_scene_id
            .expect("drain must create a portal tile in the scene");
        let content = tile_markdown(&scene, tile_id);
        // Literal marker text mirrors resident_grpc.rs's private
        // `PORTAL_UNREAD_DIVIDER_LINE` const, which is not reachable from this crate.
        assert!(
            content.contains("─── unread ───"),
            "the in-transcript unread divider must survive drain-then-render \
             for a nonzero visible unread count; expected a \
             `─── unread ───` marker, got content: {content:?}"
        );
    }

    /// Regression guard (hud-lylbz): the ambient unread-output-count indicator
    /// must survive a forced degraded repaint that fires in the SAME
    /// `drain_inner` cycle as the drain that produced it.
    ///
    /// hud-meqet carried `unread_output_count` forward across the due-loop's
    /// own drain-then-render (see `drain_then_render_surfaces_live_unread_count`
    /// above), and hud-n95zc later carried `visible_unread_output_count`
    /// forward across the degraded-repaint pass too — but left the ambient
    /// field out of that second fix. So when a connection drop lands between a
    /// publish and the drain that consumes it, the due-loop drains the publish
    /// (zeroing the session and painting the live count), then the
    /// degraded-repaint pass — flagged before this cycle started — re-derives
    /// state from the now-zeroed session and repaints the tile AGAIN, this
    /// time reading a stale `Some(0)`. The final, visible tile paint is
    /// whichever render happens last, so the ambient indicator is lost even
    /// though the divider (hud-n95zc's field) survives. Fails before the fix
    /// (final content omits "3 unread"), passes after.
    #[test]
    fn drain_then_degraded_repaint_same_cycle_preserves_ambient_unread_count() {
        use tze_hud_projection::{AdapterGeometrySnapshot, AdapterPortalRect, ProjectionBounds};

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_degraded_unread"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let proj_id = "unread-degraded-proj";

        // Attach.
        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: proj_id.to_string(),
            display_name: "Unread Degraded Projection".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let owner_token = attach_rx
            .try_recv()
            .expect("reply must be sent synchronously")
            .expect("Attach must be accepted");

        // Publish three distinct (un-coalesced) outputs → unread_output_count = 3,
        // queued as a due coalescer update that has NOT been drained yet.
        for i in 0..3 {
            let (pub_tx, mut pub_rx) =
                tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
            driver.dispatch_portal_op(PortalOp::PublishOutput {
                projection_id: proj_id.to_string(),
                owner_token: owner_token.clone(),
                output_text: format!("line-{i}"),
                logical_unit_id: None,
                output_kind: None,
                content_classification: None,
                coalesce_key: None,
                expects_reply: None,
                reply: pub_tx,
            });
            pub_rx
                .try_recv()
                .expect("publish reply must be sent synchronously")
                .expect("PublishOutput must be accepted");
        }

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.authority_mut().push_geometry_snapshot(
            proj_id,
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: 200,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        // Latch the connection degraded BEFORE the drain that consumes the
        // still-pending publish, so the forced degraded-repaint pass fires in
        // the SAME `drain_inner` cycle as the due-loop drain — the production
        // race hud-lylbz targets (a drop landing between a publish and the
        // drain that consumes it).
        assert!(
            driver.mark_projection_disconnected_at(proj_id, 150),
            "the drop must latch the entry disconnected and flag a degraded repaint"
        );
        assert!(
            driver
                .drive
                .entries
                .get(proj_id)
                .expect("drive entry must exist after attach")
                .needs_degraded_repaint,
            "the drop must flag the entry for a forced degraded repaint"
        );

        // Single drain: the due-loop drains the pending publish (zeroing the
        // session and painting "3 unread"), THEN — still within this call —
        // the forced degraded-repaint pass re-derives state from the
        // now-zeroed session and repaints the tile again.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        // Confirm the degraded pass actually fired in this cycle (one-shot
        // flag consumed) — otherwise this test would pass vacuously without
        // ever exercising the degraded-repaint carry-forward.
        assert!(
            !driver
                .drive
                .entries
                .get(proj_id)
                .expect("drive entry must still exist after drain")
                .needs_degraded_repaint,
            "the forced degraded repaint must have consumed its one-shot flag \
             within this same drain cycle"
        );

        // The FINAL painted tile (post degraded-repaint, which runs last and
        // wins) must still carry the live drained count — not the stale
        // `Some(0)` the degraded pass would otherwise re-derive from the
        // now-zeroed session.
        let tile_id = driver
            .drive
            .entries
            .get(proj_id)
            .expect("drive entry must still exist after drain")
            .tile_scene_id
            .expect("drain must create a portal tile in the scene");
        let content = tile_markdown(&scene, tile_id);
        assert!(
            content.contains("3 unread"),
            "the ambient unread indicator must survive a same-cycle \
             degraded repaint; expected a `3 unread` line, got content: {content:?}"
        );
    }

    /// Regression guard (hud-obw3q): a cooperative portal must render even when
    /// `scene.active_tab` is `None` — e.g. a config whose default tab carries no
    /// widgets boots with no active tab (`windowed/lifecycle.rs`). Before the
    /// fix, `CreatePortalTile` deferred AND the coalesced update (already
    /// consumed by `take_due_portal_update`) was dropped, so an accepted publish
    /// painted nothing. The driver must instead activate an existing tab (or
    /// create one) and paint the content there.
    #[test]
    fn drain_with_no_active_tab_activates_tab_and_paints() {
        use tze_hud_projection::{AdapterGeometrySnapshot, AdapterPortalRect, ProjectionBounds};

        // Build a driver with one attached projection that has published `text`
        // and a geometry snapshot, ready to drain.
        fn make_driver(proj: &str, text: &str) -> InProcessPortalDriver {
            let mut driver = InProcessPortalDriver {
                authority: ProjectionAuthority::new(ProjectionBounds {
                    max_portal_updates_per_second: 100,
                    ..ProjectionBounds::default()
                })
                .unwrap(),
                drive: InProcessPortalDriveState::new(),
                lease_id: None,
                portal_publish_to_present_latency: LatencyBucket::new("test_no_tab"),
                drain_deferral_count: 0,
                resident_grpc_bridge_tx: None,
            };
            let (atx, mut arx) =
                tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
            driver.dispatch_portal_op(PortalOp::Attach {
                projection_id: proj.to_string(),
                display_name: "P".to_string(),
                idempotency_key: None,
                provider_kind: None,
                content_classification: None,
                workspace_hint: None,
                repository_hint: None,
                icon_profile_hint: None,
                hud_target: None,
                reply: atx,
            });
            let token = arx
                .try_recv()
                .expect("attach reply")
                .expect("attach accepted");
            let (ptx, mut prx) = tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
            driver.dispatch_portal_op(PortalOp::PublishOutput {
                projection_id: proj.to_string(),
                owner_token: token,
                output_text: text.to_string(),
                logical_unit_id: None,
                output_kind: None,
                content_classification: None,
                coalesce_key: None,
                expects_reply: None,
                reply: ptx,
            });
            prx.try_recv()
                .expect("publish reply")
                .expect("publish accepted");
            driver.authority_mut().push_geometry_snapshot(
                proj,
                AdapterGeometrySnapshot {
                    rect: AdapterPortalRect {
                        x_px: 0,
                        y_px: 0,
                        width_px: 600,
                        height_px: 200,
                    },
                    gesture_active: false,
                    sequence: 1,
                },
            );
            driver
        }

        let assert_painted =
            |driver: &InProcessPortalDriver, scene: &SceneGraph, proj: &str, marker: &str| {
                let tile_id = driver
                    .drive
                    .entries
                    .get(proj)
                    .expect("drive entry")
                    .tile_scene_id
                    .expect("drain must create a portal tile even with no active tab");
                let content = tile_markdown(scene, tile_id);
                assert!(
                    content.contains(marker),
                    "content {marker:?} not painted; got {content:?}"
                );
            };

        // Case 1: a tab exists (the config's default "Main") but is NOT active —
        // the driver must activate it and paint there.
        {
            const MARK: &str = "NO-ACTIVE-TAB-MARKER-1";
            let mut driver = make_driver("p1", MARK);
            let mut scene = SceneGraph::new(1920.0, 1080.0);
            let main = scene.create_tab("Main", 0).unwrap();
            scene.active_tab = None; // simulate widget-less default-tab boot
            let mut processor = InputProcessor::new();
            driver.drain_inner(&mut scene, &mut processor, None, 200);
            assert_eq!(
                scene.active_tab,
                Some(main),
                "the existing tab must be activated, not left None"
            );
            assert_painted(&driver, &scene, "p1", MARK);
        }

        // Case 2: NO tabs at all — the driver must create + activate one and paint.
        {
            const MARK: &str = "NO-TABS-MARKER-2";
            let mut driver = make_driver("p2", MARK);
            let mut scene = SceneGraph::new(1920.0, 1080.0);
            assert!(scene.tabs.is_empty());
            assert_eq!(scene.active_tab, None);
            let mut processor = InputProcessor::new();
            driver.drain_inner(&mut scene, &mut processor, None, 200);
            assert!(
                scene.active_tab.is_some(),
                "a default tab must be created and activated"
            );
            assert_eq!(scene.tabs.len(), 1, "exactly one default tab created");
            assert_painted(&driver, &scene, "p2", MARK);
        }
    }

    /// Regression guard (hud-zccuf): if `create_tile` fails in the
    /// `CreatePortalTile` drain arm, the deferred tab activation must NOT have
    /// been committed — `scene.active_tab` stays unchanged for that cycle.
    ///
    /// We trigger a `create_tile` failure by using a 1×1 scene: the default
    /// portal bounds (~720×360) exceed the display area, so `create_tile`
    /// returns `BoundsOutOfRange` without creating a tile.  The projection stays
    /// attached and self-heals on the next publish once the display area is
    /// corrected.
    #[test]
    fn create_portal_tile_failure_does_not_mutate_active_tab() {
        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_deferred_tab_activation"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let proj = "proj-deferred-tab-activation";
        let token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());
        publish(&mut driver, proj, &token, "hello", 100);

        // 1×1 scene: create_tile will fail (bounds ~720×360 exceed display area).
        // Tab exists but active_tab is None — the hud-obw3q no-active-tab-on-boot
        // scenario where deferred activation matters.
        let mut scene = SceneGraph::new(1.0, 1.0);
        let main_tab = scene.create_tab("Main", 0).unwrap();
        scene.active_tab = None; // simulate widget-less default-tab boot
        let mut processor = InputProcessor::new();

        driver.drain_inner(&mut scene, &mut processor, None, 200);

        // create_tile failed → the deferred switch_active_tab must NOT have fired.
        assert_eq!(
            scene.active_tab, None,
            "active_tab must not be mutated when create_tile fails (hud-zccuf)"
        );
        assert!(
            scene.tabs.contains_key(&main_tab),
            "the existing tab must still be in the scene"
        );
        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .and_then(|e| e.tile_scene_id)
                .is_none(),
            "tile_scene_id must remain None when create_tile fails"
        );
    }

    /// Regression guard: an idempotent re-attach (same `projection_id` +
    /// matching `idempotency_key`) must NOT reset the drive entry, otherwise
    /// the next drain creates a duplicate tile and orphans the original
    /// (gemini PR #765 HIGH finding).
    ///
    /// The authority returns `accepted=true` for the replay without recreating
    /// the session; `dispatch_portal_op` must therefore preserve the existing
    /// `DriveEntry` (with its live `tile_scene_id`) instead of inserting a
    /// fresh one.
    #[test]
    fn idempotent_reattach_does_not_duplicate_tile() {
        use tze_hud_projection::{AdapterGeometrySnapshot, AdapterPortalRect, ProjectionBounds};

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_reattach"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        // ── Step 1: First attach (with an idempotency key) ─────────────────────
        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "reattach-proj".to_string(),
            display_name: "Reattach Projection".to_string(),
            idempotency_key: Some("key-1".to_string()),
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let owner_token = attach_rx
            .try_recv()
            .expect("reply must be sent synchronously")
            .expect("first Attach must be accepted");

        // ── Step 2: Publish + drain so a tile is materialised ──────────────────
        let (pub_tx, mut pub_rx) = tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: "reattach-proj".to_string(),
            owner_token: owner_token.clone(),
            output_text: "first line".to_string(),
            logical_unit_id: None,
            output_kind: None,
            content_classification: None,
            coalesce_key: None,
            expects_reply: None,
            reply: pub_tx,
        });
        pub_rx
            .try_recv()
            .expect("publish reply must be sent synchronously")
            .expect("PublishOutput must be accepted");

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.authority_mut().push_geometry_snapshot(
            "reattach-proj",
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: 200,
                },
                gesture_active: false,
                sequence: 1,
            },
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let original_tile = driver
            .drive
            .entries
            .get("reattach-proj")
            .expect("drive entry must exist after first attach+drain")
            .tile_scene_id
            .expect("first drain must create a portal tile");
        let tile_count_after_first = scene.tile_count();

        // ── Step 3: Idempotent re-attach (same id + same key) ──────────────────
        let (re_tx, mut re_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "reattach-proj".to_string(),
            display_name: "Reattach Projection".to_string(),
            idempotency_key: Some("key-1".to_string()),
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: re_tx,
        });
        let replay_token = re_rx
            .try_recv()
            .expect("reply must be sent synchronously")
            .expect("idempotent re-attach must be accepted");
        assert_ne!(
            replay_token, owner_token,
            "idempotent re-attach must rotate the owner token"
        );

        let (stale_tx, mut stale_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishStatus {
            projection_id: "reattach-proj".to_string(),
            owner_token,
            lifecycle_state: "active".to_string(),
            status_text: Some("stale token".to_string()),
            reply: stale_tx,
        });
        let stale_rejection = stale_rx
            .try_recv()
            .expect("stale-token reply must be sent synchronously")
            .expect_err("pre-replay token must be invalidated");
        assert_eq!(
            stale_rejection.error_code,
            ProjectionErrorCode::ProjectionUnauthorized
        );

        let (current_tx, mut current_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishStatus {
            projection_id: "reattach-proj".to_string(),
            owner_token: replay_token,
            lifecycle_state: "active".to_string(),
            status_text: Some("current token".to_string()),
            reply: current_tx,
        });
        current_rx
            .try_recv()
            .expect("current-token reply must be sent synchronously")
            .expect("replay token must authorize owner operations");

        // The drive entry must still point at the SAME tile — not reset to None.
        let tile_after_reattach = driver
            .drive
            .entries
            .get("reattach-proj")
            .expect("drive entry must survive idempotent re-attach")
            .tile_scene_id
            .expect("idempotent re-attach must preserve the existing tile_scene_id");
        assert_eq!(
            tile_after_reattach, original_tile,
            "idempotent re-attach must NOT reset tile_scene_id; resetting it makes \
             the next drain create a duplicate tile and orphan the original"
        );

        // ── Step 4: Another drain must NOT create a second tile ────────────────
        driver.drain_inner(
            &mut scene,
            &mut processor,
            Some(tab_id),
            PORTAL_UPDATE_RATE_WINDOW_WALL_US * 2 + 1,
        );
        assert_eq!(
            scene.tile_count(),
            tile_count_after_first,
            "idempotent re-attach + drain must not create a duplicate portal tile"
        );
    }

    // ── hud-66i1s: height-only head-trim condition (runtime parity) ───────────

    /// Regression test for hud-66i1s: the in-process runtime drain must call
    /// `notify_head_content_removed` when `new_content_height_px` shrinks between
    /// consecutive `RenderPortal` drains, even when `visible_transcript_bytes` does
    /// NOT shrink (or grows).
    ///
    /// This is the runtime-path counterpart to the CLI-path test
    /// `head_trim_geometry_emitted_when_height_shrinks_without_byte_shrink` in
    /// `projection_authority.rs` (PR #779 / hud-hkaw2).
    ///
    /// The old dual-condition (`bytes_shrank && height_shrank`) would miss the
    /// case where a many-newline unit is evicted by a flat unit of equal or
    /// greater byte count.  The fixed height-only condition fires correctly.
    ///
    /// Byte/line layout (max_vis=30B, 1-line viewport):
    ///
    ///   drain1 (CreatePortalTile): publish seed unit "S" (1B, 1 line).
    ///   drain2 (RenderPortal): publish "\n" × 24 = 24B, 25 lines.
    ///     Visible window: 1B + 24B = 25B ≤ 30B → both visible.
    ///     Total height: 26 lines × line_h. Tile is AtTail; scroll_y = 25 × line_h.
    ///   [user scrolls back 1 px: tile becomes ScrolledBack, scroll_y = 25*line_h - 1]
    ///   drain3 (RenderPortal): publish flat "B" × 30 = 30B, 1 line.
    ///     Visible window from tail: 30B ≤ 30B → fits; + 24B = 54B > 30 → evicted.
    ///     visible_bytes = 30 (GREW from 25), height = 1 × line_h (SHRANK from 26).
    ///     height-only condition: height_shrank → notify_head_content_removed fires.
    ///     Removed height = (26-1)*line_h; ScrolledBack offset decreases to 0 (clamped).
    ///     Old dual-condition: bytes_shrank=(30<25)=false → would NOT fire (bug).
    #[test]
    fn head_trim_fires_on_height_shrink_without_byte_shrink_runtime_path() {
        use tze_hud_input::ScrollEvent;
        use tze_hud_scene::TileScrollConfig;

        // max_vis = 30B so that:
        //   seed(1B) + many-lines(24B) = 25B ≤ 30B → both fit in drain2;
        //   flat(30B) alone ≤ 30B → fits; flat + many-lines = 54B > 30 → evicts many-lines.
        let max_vis: usize = 30;
        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                max_visible_transcript_bytes: max_vis,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_htrim_runtime"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let token = attach_and_get_token(&mut driver, "proj-htrim");
        driver.attach_projection("proj-htrim", Vec::new());

        // Derive line height from the adapter's font token.
        let font_size_px = driver
            .drive
            .entries
            .get("proj-htrim")
            .unwrap()
            .adapter
            .visual_tokens()
            .transcript_font_size_px;
        let line_h = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER;

        // Viewport = 1 line so that after drain2 (26 lines) there is real overflow
        // and the tile is AtTail with scroll_y = 25 × line_h.
        let viewport_h = (1.0_f32 * line_h).ceil();

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // drain1 (CreatePortalTile): seed unit "S" = 1B, 1 line.
        publish(&mut driver, "proj-htrim", &token, "S", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let tile_id = driver
            .drive
            .entries
            .get("proj-htrim")
            .expect("drive entry must exist after drain1")
            .tile_scene_id
            .expect("tile must be created after drain1 (CreatePortalTile)");

        // Register scroll config for hit-test (needed for process_scroll_event).
        scene
            .register_tile_scroll_config(
                tile_id,
                TileScrollConfig {
                    scrollable_x: false,
                    scrollable_y: true,
                    content_width: None,
                    content_height: None,
                },
            )
            .unwrap();

        // Resize tile to the 1-line viewport so overflow and follow-tail are
        // detectable.
        let _ = scene.update_tile_bounds(
            tile_id,
            Rect::new(0.0, 0.0, 600.0, viewport_h),
            PORTAL_DRIVER_NAMESPACE,
        );

        // drain2 (RenderPortal): many-lines unit "\n" × 24 = 24B, 25 lines.
        // After drain2: visible = [seed(1B) + many-lines(24B)] = 25B ≤ 30B;
        // total height = 26 × line_h; tile is AtTail with scroll_y = 25 × line_h.
        let many_lines = "\n".repeat(24);
        assert_eq!(many_lines.len(), 24, "many_lines must be exactly 24B");

        let ts2 = PORTAL_UPDATE_RATE_WINDOW_WALL_US + 25;
        publish(&mut driver, "proj-htrim", &token, &many_lines, ts2);
        let drain2_now = ts2 + PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), drain2_now);

        // After drain2 the tile must be at-tail (26 lines in 1-line viewport).
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "tile must be at-tail after drain2 (26-line content in 1-line viewport)"
        );

        // Scroll back by 1 px to make the tile ScrolledBack.
        // The tile occupies (0,0)→(600, viewport_h); use the tile centre for hit-testing.
        let centre_y = viewport_h / 2.0;
        let scroll_ev = ScrollEvent {
            x: 300.0,
            y: centre_y,
            delta_x: 0.0,
            delta_y: -1.0, // negative = scroll up (move content down = scroll back toward head)
        };
        processor.process_scroll_event(&scroll_ev, &mut scene);

        // Tile must now be ScrolledBack (no longer at-tail).
        assert!(
            !scene.tile_follow_tail_at_tail(tile_id),
            "tile must be ScrolledBack after user scroll-back event"
        );

        // Record pre-drain3 scroll offset.
        let (_, pre_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            pre_y > 0.0,
            "scroll offset must be positive before drain3 (got {pre_y})"
        );

        // drain3 (RenderPortal): flat unit "B" × 30 = 30B, 1 line.
        // Visible from tail: flat(30B) ≤ 30B → fits; flat + many-lines = 54B > 30 → evicted.
        // visible_bytes = 30 (GREW from 25); total height = 1 × line_h (SHRANK from 26×line_h).
        //
        // KEY: visible_transcript_bytes grew (25 → 30), so the old dual-condition
        // (bytes_shrank && height_shrank) would NOT fire — the bug.
        // The fixed height-only condition (height_shrank) MUST fire.
        let flat_unit = "B".repeat(30);
        assert_eq!(flat_unit.len(), 30, "flat_unit must be exactly 30B");

        let ts3 = drain2_now + PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1;
        publish(&mut driver, "proj-htrim", &token, &flat_unit, ts3);
        let drain3_now = ts3 + PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), drain3_now);

        // Flush the dirty scroll state to the scene (notify_head_content_removed
        // marks the tile dirty for ScrolledBack tiles; commit_scroll_updates
        // applies the updated offset to the scene graph).
        processor.commit_scroll_updates(&mut scene);

        // KEY ASSERTION (hud-66i1s fix):
        // The scroll offset must have decreased because head-trim fired.
        // notify_head_content_removed(removed_px = 25 × line_h) shifts the
        // ScrolledBack offset down by 25 × line_h → clamped to 0 (since
        // new max_scroll_offset = 1×line_h - 1×line_h = 0).
        //
        // If the old dual-condition were still in place:
        //   bytes_shrank = (30 < 25) = false → no head-trim call → offset unchanged → BUG.
        // With the height-only fix:
        //   height_shrank = (1×line_h < 26×line_h) = true → head-trim fires → offset = 0.
        let (_, post_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            post_y < pre_y,
            "hud-66i1s regression: scroll offset must decrease after height-only head-trim; \
             pre_y={pre_y} post_y={post_y} line_h={line_h}. \
             If this fails, notify_head_content_removed was not called — the old dual-condition \
             (bytes_shrank && height_shrank) is still in place at the runtime drain site."
        );
        // The new content has the same height as the viewport so max_scroll = 0.
        // After head-trim adjustment the offset must reach 0.
        assert!(
            post_y <= f32::EPSILON,
            "hud-66i1s: after head-trim the max scroll offset is 0 (1-line content in 1-line \
             viewport); offset must clamp to 0, got post_y={post_y}"
        );
    }

    // ── Portal observability surface tests (hud-bq0gl.14) ────────────────────

    /// Regression guard: `drain_deferral_count` must be incremented when the
    /// drain loop encounters a portal whose rate window has not yet elapsed
    /// (i.e. `take_due_portal_update` returns `Ok(None)`).
    ///
    /// The rate window opens on the FIRST successful drain (at `now_us = T1`).
    /// A second publish + drain within `T1 + PORTAL_UPDATE_RATE_WINDOW_WALL_US`
    /// triggers the `Ok(None)` path because `portal_update_allowed` returns
    /// `false` once `max_portal_updates_per_second` is exhausted.
    ///
    /// We use `max_portal_updates_per_second: 1` (one update allowed per window)
    /// so a single publish + drain pair saturates the window, and the next
    /// drain in the same window is deferred.
    #[test]
    fn drain_deferral_count_increments_on_rate_window_not_elapsed() {
        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 1,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_deferral"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let mut processor = InputProcessor::new();
        let tab_id = scene.create_tab("Main", 0).unwrap();

        let token = attach_and_get_token(&mut driver, "proj-defer");
        driver.attach_projection("proj-defer", Vec::new());

        assert_eq!(
            driver.drain_deferral_count(),
            0,
            "deferral count must be 0 before any drain"
        );

        // First publish + drain at T1: opens the rate window, returns Ok(Some(_)).
        // max_portal_updates_per_second = 1, so the window is now saturated.
        let t1 = PORTAL_UPDATE_RATE_WINDOW_WALL_US;
        publish(&mut driver, "proj-defer", &token, "hello", t1);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), t1);

        assert_eq!(
            driver.drain_deferral_count(),
            0,
            "deferral count must still be 0 after successful drain"
        );

        // Second publish within the same rate window (T1 + 1, window end = T1 + 1_000_000).
        // The next drain at T1 + 2 is within the window; take_due_portal_update
        // returns Ok(None) because max_portal_updates_per_second = 1 is saturated.
        let t2 = t1 + 1;
        publish(&mut driver, "proj-defer", &token, "world", t2);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), t2);

        assert_eq!(
            driver.drain_deferral_count(),
            1,
            "deferral count must be 1 after a drain where rate-window has not elapsed"
        );

        // A second deferred drain must increment again.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), t2 + 1);
        assert_eq!(
            driver.drain_deferral_count(),
            2,
            "deferral count must increment on each rate-limited drain cycle"
        );
    }

    /// `drain_deferral_count` must NOT increment when a drain successfully
    /// materialises an update (rate window has elapsed).
    #[test]
    fn drain_deferral_count_unchanged_on_successful_drain() {
        let mut driver = InProcessPortalDriver::new();
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let mut processor = InputProcessor::new();
        let tab_id = scene.create_tab("Main", 0).unwrap();

        let token = attach_and_get_token(&mut driver, "proj-ok");
        driver.attach_projection("proj-ok", Vec::new());

        publish(&mut driver, "proj-ok", &token, "content", 1);

        // Drain past the rate window — update is materialised successfully.
        let now = PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1_000_000;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), now);

        assert_eq!(
            driver.drain_deferral_count(),
            0,
            "deferral count must remain 0 after a successful drain"
        );
    }

    /// `coalescer_portal_count` on `ProjectionAuthority` must return the number
    /// of portals registered in the cadence coalescer after attach.
    #[test]
    fn coalescer_portal_count_reflects_attached_portals() {
        let mut driver = InProcessPortalDriver::new();

        assert_eq!(
            driver.authority_mut().coalescer_portal_count(),
            0,
            "coalescer_portal_count must be 0 before any portal is attached"
        );

        attach_and_get_token(&mut driver, "proj-c1");
        let _ = driver.authority_mut().coalescer_portal_count(); // idempotent reads

        // After a publish the coalescer registers the portal.
        let token = attach_and_get_token(&mut driver, "proj-c2");
        publish(&mut driver, "proj-c2", &token, "hi", 1);

        assert_eq!(
            driver.authority_mut().coalescer_portal_count(),
            1,
            "coalescer_portal_count must be 1 after one portal publishes"
        );
    }

    /// `coalescer_total_taken` must increment each time a successful drain
    /// consumes a pending coalescer snapshot.
    #[test]
    fn coalescer_total_taken_increments_on_drain() {
        let mut driver = InProcessPortalDriver::new();
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let mut processor = InputProcessor::new();
        let tab_id = scene.create_tab("Main", 0).unwrap();

        let token = attach_and_get_token(&mut driver, "proj-tk");
        driver.attach_projection("proj-tk", Vec::new());

        assert_eq!(
            driver.authority_mut().coalescer_total_taken(),
            0,
            "total_taken must be 0 before any drain"
        );

        // First publish + drain.
        publish(&mut driver, "proj-tk", &token, "a", 1);
        let now1 = PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1_000_000;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), now1);

        assert_eq!(
            driver.authority_mut().coalescer_total_taken(),
            1,
            "total_taken must be 1 after one successful drain"
        );

        // Second publish + drain.
        let ts2 = now1 + 1;
        publish(&mut driver, "proj-tk", &token, "b", ts2);
        let now2 = ts2 + PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1_000_000;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), now2);

        assert_eq!(
            driver.authority_mut().coalescer_total_taken(),
            2,
            "total_taken must be 2 after two successful drains"
        );
    }

    /// Regression guard (hud-bsr7u): the drain loop must TERMINATE when the
    /// coalescer holds a pending entry for a projection whose session is gone
    /// (session/coalescer divergence — e.g. operator cleanup that purged only
    /// the session).
    ///
    /// Mechanism of the original freeze:
    ///   1. `next_due_projection_id` returns the orphaned id (pending entry set).
    ///   2. `take_due_portal_update` fails the session lookup and returns
    ///      `Err(ProjectionNotFound)` BEFORE it can consume the coalescer entry.
    ///   3. The `Err` arm previously did `detach + continue` without touching the
    ///      coalescer → `next_due_projection_id` returns the SAME id again →
    ///      unbounded loop that wedges the event loop.
    ///
    /// The fix discards the coalescer entry in the `Err` arm. This test injects
    /// an orphaned coalescer entry (no session), runs a drain, and asserts the
    /// drain RETURNS (does not hang) and that the orphaned entry was consumed so
    /// it cannot be returned again. The bounded `#[test]` harness will hang and
    /// time out if the loop spins — termination IS the assertion.
    #[test]
    fn drain_terminates_on_orphaned_coalescer_entry() {
        let mut driver = InProcessPortalDriver::new();
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let mut processor = InputProcessor::new();
        let tab_id = scene.create_tab("Main", 0).unwrap();

        // Inject an orphaned coalescer entry: a pending snapshot with NO session
        // backing it (the abnormal divergence state operator-cleanup used to leave).
        driver
            .authority_mut()
            .inject_orphan_coalescer_entry_for_test("proj-orphan");
        assert_eq!(
            driver.authority_mut().coalescer_pending_portal_count(),
            1,
            "precondition: orphaned coalescer entry must be pending"
        );

        // This must NOT hang. The Err arm of the drain loop consumes the
        // orphaned entry; the loop then sees an empty coalescer and breaks.
        let now = PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1_000_000;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), now);

        // The orphaned entry must be gone so `next_due_projection_id` cannot
        // return it again on the next tick.
        assert_eq!(
            driver.authority_mut().coalescer_pending_portal_count(),
            0,
            "hud-bsr7u: drain Err-arm must consume the orphaned coalescer entry"
        );
        assert!(
            driver.authority_mut().next_due_projection_id().is_none(),
            "hud-bsr7u: no portal may be reported due after the orphaned entry is consumed"
        );

        // A second drain is a clean no-op (idempotent termination).
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), now + 1);
        assert_eq!(
            driver.authority_mut().coalescer_pending_portal_count(),
            0,
            "second drain must remain idle"
        );
    }

    /// Regression guard (hud-bsr7u): operator cleanup performed through the
    /// driver's hosted authority leaves the coalescer with no pending entry, and
    /// a subsequent drain terminates cleanly.
    ///
    /// This exercises the full path the live freeze took: cooperative attach →
    /// publish_output (seeds a coalescer pending entry) → operator cleanup →
    /// drain. With the layer-1 fix the coalescer is already clear at cleanup
    /// time; with the layer-2 fix the drain would still terminate even if it
    /// weren't. Together they guarantee no busy-spin.
    #[test]
    fn operator_cleanup_then_drain_does_not_spin() {
        use tze_hud_projection::{CleanupAuthority, CleanupRequest};

        let mut driver = InProcessPortalDriver::new();
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let mut processor = InputProcessor::new();
        let tab_id = scene.create_tab("Main", 0).unwrap();

        driver
            .authority_mut()
            .set_operator_authority("operator-secret")
            .unwrap();

        let token = attach_and_get_token(&mut driver, "proj-op");
        driver.attach_projection("proj-op", Vec::new());

        // Seed a pending coalescer entry without draining it.
        publish(&mut driver, "proj-op", &token, "live transcript", 100);
        assert_eq!(
            driver.authority_mut().coalescer_pending_portal_count(),
            1,
            "precondition: publish seeds a pending coalescer entry"
        );

        // Operator cleanup — the live freeze trigger.
        let resp = driver.authority_mut().handle_cleanup(
            CleanupRequest {
                envelope: OperationEnvelope {
                    operation: ProjectionOperation::Cleanup,
                    projection_id: "proj-op".to_string(),
                    request_id: "req-op-cleanup".to_string(),
                    client_timestamp_wall_us: 1,
                },
                cleanup_authority: CleanupAuthority::Operator,
                owner_token: None,
                operator_authority: Some("operator-secret".to_string()),
                reason: "operator override".to_string(),
            },
            "operator",
            200,
        );
        assert!(resp.accepted, "operator cleanup must be accepted");

        // Layer-1: coalescer already purged at cleanup time.
        assert_eq!(
            driver.authority_mut().coalescer_pending_portal_count(),
            0,
            "hud-bsr7u layer-1: operator cleanup must purge the coalescer entry"
        );

        // Drain must terminate (no busy-spin) and remain idle.
        let now = 200 + PORTAL_UPDATE_RATE_WINDOW_WALL_US + 1_000_000;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), now);
        assert!(
            driver.authority_mut().next_due_projection_id().is_none(),
            "hud-bsr7u: no portal may remain due after operator cleanup + drain"
        );
    }

    // ── hud-m7w3g: publish classification + coalesce_key threading ────────────

    #[test]
    fn parse_output_kind_defaults_and_variants() {
        assert_eq!(parse_output_kind(None), Ok(OutputKind::Assistant));
        assert_eq!(
            parse_output_kind(Some("assistant")),
            Ok(OutputKind::Assistant)
        );
        assert_eq!(parse_output_kind(Some("tool")), Ok(OutputKind::Tool));
        assert_eq!(parse_output_kind(Some("status")), Ok(OutputKind::Status));
        assert_eq!(parse_output_kind(Some("error")), Ok(OutputKind::Error));
        assert_eq!(parse_output_kind(Some("other")), Ok(OutputKind::Other));
        // "viewer" is reserved for viewer-submitted input; external agents must
        // not be able to publish it.
        let viewer_err = parse_output_kind(Some("viewer")).unwrap_err();
        assert!(
            viewer_err.contains("reserved"),
            "expected 'reserved' in error, got: {viewer_err}"
        );
    }

    #[test]
    fn parse_output_kind_rejects_unknown() {
        let err = parse_output_kind(Some("Assistant")).unwrap_err();
        assert!(err.contains("invalid output_kind"), "got: {err}");
        // Camel-case / unknown spellings are rejected, not silently defaulted.
        assert!(parse_output_kind(Some("bogus")).is_err());
    }

    #[test]
    fn parse_content_classification_defaults_safe_to_private() {
        // Omitted classification must default to Private (privacy safe-by-default).
        assert_eq!(
            parse_content_classification(None),
            Ok(ContentClassification::Private)
        );
        assert_eq!(
            parse_content_classification(Some("public")),
            Ok(ContentClassification::Public)
        );
        assert_eq!(
            parse_content_classification(Some("household")),
            Ok(ContentClassification::Household)
        );
        assert_eq!(
            parse_content_classification(Some("private")),
            Ok(ContentClassification::Private)
        );
        assert_eq!(
            parse_content_classification(Some("sensitive")),
            Ok(ContentClassification::Sensitive)
        );
    }

    #[test]
    fn parse_content_classification_rejects_unknown() {
        let err = parse_content_classification(Some("secret")).unwrap_err();
        assert!(err.contains("invalid content_classification"), "got: {err}");
    }

    // ── hud-acy4o: attach identity fields round-trip / defaults / rejection ────

    #[test]
    fn parse_provider_kind_defaults_and_variants() {
        assert_eq!(parse_provider_kind(None), Ok(ProviderKind::Other));
        assert_eq!(parse_provider_kind(Some("codex")), Ok(ProviderKind::Codex));
        assert_eq!(
            parse_provider_kind(Some("claude")),
            Ok(ProviderKind::Claude)
        );
        assert_eq!(
            parse_provider_kind(Some("opencode")),
            Ok(ProviderKind::Opencode)
        );
        assert_eq!(parse_provider_kind(Some("other")), Ok(ProviderKind::Other));
    }

    #[test]
    fn parse_provider_kind_rejects_unknown() {
        let err = parse_provider_kind(Some("Claude")).unwrap_err();
        assert!(err.contains("invalid provider_kind"), "got: {err}");
        assert!(parse_provider_kind(Some("gpt4")).is_err());
    }

    /// Identity fields supplied at attach must round-trip into the authority
    /// session state (hud-acy4o).
    ///
    /// Verifies:
    /// - `provider_kind=claude` is reflected in `projection_identity`.
    /// - `content_classification=household` is reflected in the identity summary.
    /// - `workspace_hint` and `icon_profile_hint` pass through as-is.
    /// - Omitted fields keep safe defaults (`classification` → `private`).
    #[test]
    fn dispatch_attach_identity_fields_round_trip() {
        use tze_hud_projection::{ContentClassification, ProjectionBounds, ProviderKind};

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds::default()).unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_attach_identity"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        // Attach with explicit identity fields.
        let (tx, mut rx) = tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "id-proj".to_string(),
            display_name: "Identity Test".to_string(),
            idempotency_key: None,
            provider_kind: Some("claude".to_string()),
            content_classification: Some("household".to_string()),
            workspace_hint: Some("/home/agent/project".to_string()),
            repository_hint: Some("https://github.com/org/repo".to_string()),
            icon_profile_hint: Some("claude-code".to_string()),
            hud_target: Some("primary".to_string()),
            reply: tx,
        });
        rx.try_recv()
            .expect("reply sent synchronously")
            .expect("attach must be accepted");

        let identity = driver
            .authority_mut()
            .projection_identity("id-proj")
            .expect("identity must exist after successful attach");
        assert_eq!(
            identity.provider_kind,
            ProviderKind::Claude,
            "provider_kind=claude must be reflected in session identity"
        );
        assert_eq!(
            identity.content_classification,
            ContentClassification::Household,
            "content_classification=household must be reflected in session identity"
        );

        // Attach with omitted optional fields — classification must default to private.
        let (tx2, mut rx2) = tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "id-proj-defaults".to_string(),
            display_name: "Defaults Test".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: tx2,
        });
        rx2.try_recv()
            .expect("reply sent synchronously")
            .expect("attach with omitted fields must be accepted");

        let identity2 = driver
            .authority_mut()
            .projection_identity("id-proj-defaults")
            .expect("identity must exist for defaults projection");
        assert_eq!(
            identity2.provider_kind,
            ProviderKind::Other,
            "omitted provider_kind must default to Other"
        );
        assert_eq!(
            identity2.content_classification,
            ContentClassification::Private,
            "omitted content_classification must default to Private (safe-by-default)"
        );
    }

    /// An unrecognized `provider_kind` on attach must be rejected at the driver
    /// before the authority is called (hud-acy4o).
    #[test]
    fn dispatch_attach_rejects_invalid_provider_kind() {
        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds::default()).unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_attach_invalid_kind"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let (tx, mut rx) = tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "bad-kind-proj".to_string(),
            display_name: "Bad Kind".to_string(),
            idempotency_key: None,
            provider_kind: Some("GPT-4".to_string()),
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: tx,
        });
        let err = rx
            .try_recv()
            .expect("reply sent synchronously")
            .expect_err("invalid provider_kind must be rejected");
        assert_eq!(
            err.error_code,
            ProjectionErrorCode::ProjectionInvalidArgument,
            "a driver-side validation failure carries the stable INVALID_ARGUMENT code"
        );
        assert!(
            err.message.contains("invalid provider_kind"),
            "got: {}",
            err.message
        );
        // Authority must never have seen the request.
        assert!(
            !driver.authority_mut().has_projection("bad-kind-proj"),
            "rejected attach must not create a session in the authority"
        );
    }

    /// An unrecognized `content_classification` on attach must be rejected at
    /// the driver (hud-acy4o).
    #[test]
    fn dispatch_attach_rejects_invalid_content_classification() {
        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds::default()).unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_attach_invalid_class"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let (tx, mut rx) = tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "bad-class-proj".to_string(),
            display_name: "Bad Class".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: Some("top-secret".to_string()),
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: tx,
        });
        let err = rx
            .try_recv()
            .expect("reply sent synchronously")
            .expect_err("invalid content_classification must be rejected");
        assert_eq!(
            err.error_code,
            ProjectionErrorCode::ProjectionInvalidArgument,
            "a driver-side validation failure carries the stable INVALID_ARGUMENT code"
        );
        assert!(
            err.message.contains("invalid content_classification"),
            "got: {}",
            err.message
        );
        assert!(
            !driver.authority_mut().has_projection("bad-class-proj"),
            "rejected attach must not create a session in the authority"
        );
    }

    /// An explicit, valid `output_kind` / `content_classification` plus a
    /// `coalesce_key` must thread through `dispatch_portal_op(PublishOutput)`
    /// into the authority and be accepted (hud-m7w3g).
    #[test]
    fn dispatch_publish_threads_classification_and_coalesce_key() {
        use tze_hud_projection::ProjectionBounds;

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_publish_classification"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "classify-proj".to_string(),
            display_name: "Classify Projection".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let owner_token = attach_rx
            .try_recv()
            .expect("reply sent synchronously")
            .expect("attach accepted");

        let (pub_tx, mut pub_rx) = tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: "classify-proj".to_string(),
            owner_token,
            output_text: "status line".to_string(),
            logical_unit_id: None,
            output_kind: Some("status".to_string()),
            content_classification: Some("public".to_string()),
            coalesce_key: Some("ck-1".to_string()),
            expects_reply: None,
            reply: pub_tx,
        });
        pub_rx
            .try_recv()
            .expect("reply sent synchronously")
            .expect("publish with explicit classification + coalesce_key must be accepted");
    }

    /// `expects_reply` (the `Question` signal, hud-jip0k) must thread through
    /// `dispatch_portal_op(PublishOutput)` into the authority: an explicit
    /// `Some(true)` must round-trip to `true` on the retained transcript unit,
    /// and an omitted `None` must round-trip to `false` — the exact
    /// pre-existing behavior for every caller that predates this field.
    #[test]
    fn dispatch_publish_threads_expects_reply() {
        use tze_hud_projection::{ProjectedPortalPolicy, ProjectionBounds};

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_publish_expects_reply"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "question-proj".to_string(),
            display_name: "Question Projection".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let owner_token = attach_rx
            .try_recv()
            .expect("reply sent synchronously")
            .expect("attach accepted");

        // Omitted expects_reply — must round-trip to false (backward-compat).
        let (pub_tx, mut pub_rx) = tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: "question-proj".to_string(),
            owner_token: owner_token.clone(),
            output_text: "plain output".to_string(),
            logical_unit_id: Some("unit-plain".to_string()),
            output_kind: None,
            content_classification: None,
            coalesce_key: None,
            expects_reply: None,
            reply: pub_tx,
        });
        pub_rx
            .try_recv()
            .expect("reply sent synchronously")
            .expect("publish with omitted expects_reply must be accepted");

        // Explicit expects_reply: Some(true) — must round-trip to true.
        let (pub_tx, mut pub_rx) = tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: "question-proj".to_string(),
            owner_token,
            output_text: "which option do you prefer?".to_string(),
            logical_unit_id: Some("unit-question".to_string()),
            output_kind: None,
            content_classification: None,
            coalesce_key: None,
            expects_reply: Some(true),
            reply: pub_tx,
        });
        pub_rx
            .try_recv()
            .expect("reply sent synchronously")
            .expect("publish with expects_reply: Some(true) must be accepted");

        let state = driver
            .authority_mut()
            .projected_portal_state("question-proj", &ProjectedPortalPolicy::permit_all())
            .expect("portal state materializes");
        assert_eq!(
            state.visible_transcript.len(),
            2,
            "both publishes must append distinct units"
        );
        assert!(
            !state.visible_transcript[0].expects_reply,
            "omitted expects_reply must round-trip to false"
        );
        assert!(
            state.visible_transcript[1].expects_reply,
            "expects_reply: Some(true) must round-trip to true"
        );
    }

    /// An unrecognized `content_classification` must be rejected at the driver
    /// before reaching the authority, surfaced through the reply channel —
    /// never silently coerced to a less-private default (hud-m7w3g).
    #[test]
    fn dispatch_publish_rejects_invalid_classification() {
        use tze_hud_projection::ProjectionBounds;

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds::default()).unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("test_publish_invalid"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: "reject-proj".to_string(),
            display_name: "Reject Projection".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let owner_token = attach_rx
            .try_recv()
            .expect("reply sent synchronously")
            .expect("attach accepted");

        let (pub_tx, mut pub_rx) = tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: "reject-proj".to_string(),
            owner_token,
            output_text: "secret payload".to_string(),
            logical_unit_id: None,
            output_kind: None,
            content_classification: Some("top-secret".to_string()),
            coalesce_key: None,
            expects_reply: None,
            reply: pub_tx,
        });
        let result = pub_rx
            .try_recv()
            .expect("reply sent synchronously even on rejection");
        let err = result.expect_err("invalid content_classification must be rejected");
        assert_eq!(
            err.error_code,
            ProjectionErrorCode::ProjectionInvalidArgument,
            "a driver-side validation failure carries the stable INVALID_ARGUMENT code"
        );
        assert!(
            err.message.contains("invalid content_classification"),
            "got: {}",
            err.message
        );
    }

    /// hud-pk9pz (task 4.6): a session attaching AFTER the prior portal's lease
    /// grace period has already expired must start a FRESH portal under a NEW
    /// lease — it must never revive the removed surface nor reuse the dead lease.
    ///
    /// Structural reason this needs a guard: `expire_leases` reaps an orphaned
    /// lease's tiles on grace expiry but leaves the lease entry resident in the
    /// map as `Expired` (terminal state recorded in place). The driver caches
    /// its lease id in `self.lease_id`. The OLD reuse check
    /// (`lease_capabilities(..).is_some()`) returned `Some` for that dead-but-
    /// resident lease, so the driver would reuse it; the next `create_tile` then
    /// fails its `require_active_lease` check and the re-attaching session
    /// silently gets NO portal. The fix gates reuse on `lease_is_active`, so a
    /// post-grace re-attach grants a fresh lease and creates a fresh tile.
    #[test]
    fn reattach_after_grace_expiry_starts_fresh_portal_under_new_lease() {
        use std::sync::Arc;
        use tze_hud_scene::{Clock, TestClock};

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        // Scene backed by a TestClock so we can drive lease grace expiry
        // deterministically. Scene clock (ms) is independent of the drain
        // rate-window clock (`now_us`) passed to `drain_inner`.
        let clock = TestClock::new(1_000);
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // --- First session: attach + drain → CreatePortalTile under lease L1. ---
        let token1 = attach_and_get_token(&mut driver, "proj-1");
        driver.attach_projection("proj-1", Vec::new());
        publish(&mut driver, "proj-1", &token1, "session-one content", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let lease1 = driver
            .lease_id
            .expect("driver must hold a lease after the first CreatePortalTile drain");
        let tile1 = driver
            .drive
            .entries
            .get("proj-1")
            .expect("first drive entry must exist")
            .tile_scene_id
            .expect("first tile must be created");
        assert_eq!(scene.tile_count(), 1, "first portal tile is live");
        assert!(
            scene.lease_is_active(&lease1),
            "the first lease is active while the portal is live"
        );

        // --- First session drops; grace period elapses; orphan path reaps it. ---
        scene
            .disconnect_lease(&lease1, clock.now_millis())
            .expect("disconnect orphans the lease (degraded surface, grace begins)");
        // Advance well past the 30s grace period, then run expiry.
        clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1_000);
        let expiries = scene.expire_leases();
        assert_eq!(expiries.len(), 1, "the grace-expired lease must be reaped");
        assert!(
            expiries[0].removed_tiles.contains(&tile1),
            "the dead session's surface is removed on grace expiry"
        );
        assert_eq!(
            scene.tile_count(),
            0,
            "no surface survives past grace — nothing to revive"
        );
        // The lease entry is left resident as Expired (terminal recorded in
        // place) — this is exactly the trap the fix guards against.
        assert!(
            !scene.lease_is_active(&lease1),
            "L1 is no longer active after grace expiry"
        );

        // --- Second session attaches AFTER grace expiry. It must get a FRESH ---
        // --- portal under a NEW lease, not a revived surface or reused lease.  --
        let token2 = attach_and_get_token(&mut driver, "proj-2");
        driver.attach_projection("proj-2", Vec::new());
        publish(&mut driver, "proj-2", &token2, "session-two content", 300);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 400);

        let lease2 = driver
            .lease_id
            .expect("driver must hold a lease after the re-attach CreatePortalTile drain");
        assert_ne!(
            lease2, lease1,
            "post-grace re-attach must obtain a FRESH lease, not reuse the dead L1"
        );
        assert!(
            scene.lease_is_active(&lease2),
            "the fresh lease must be Active so create_tile succeeds"
        );

        let tile2 = driver
            .drive
            .entries
            .get("proj-2")
            .expect("second drive entry must exist")
            .tile_scene_id
            .expect(
                "a fresh portal tile MUST be created for the re-attaching session — \
                 if this is None, the driver reused the dead lease and create_tile \
                 silently failed (the bug this test guards)",
            );
        assert_ne!(
            tile2, tile1,
            "the new portal is a fresh surface, not a revival of the removed one"
        );
        assert_eq!(
            scene.tile_count(),
            1,
            "exactly one live portal tile — the fresh one"
        );
    }

    /// End-to-end ingress lifecycle through `dispatch_portal_op` (hud-bq0gl.1).
    ///
    /// This is the production MCP ingress path: each `PortalOp` is the exact
    /// message the MCP tool handlers send over the portal-op channel. The test
    /// drives Attach → (seed HUD input) → GetPendingInput → AcknowledgeInput →
    /// Detach and asserts the authority state transitions at each step, plus the
    /// owner-token authorization gate.
    ///
    /// No GPU / scene drain is involved — this exercises only the op-dispatch
    /// → authority wiring that the new MCP surface depends on.
    #[test]
    fn dispatch_portal_op_input_lifecycle_end_to_end() {
        use tze_hud_projection::ContentClassification;

        let mut driver = InProcessPortalDriver::new();
        let proj = "proj-ingress";

        // 1. Attach via the op channel — yields the owner token.
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: proj.to_string(),
            display_name: "Ingress Test".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: tx,
        });
        let token = rx
            .blocking_recv()
            .expect("attach reply must arrive")
            .expect("attach must be accepted");
        assert!(!token.is_empty(), "attach must return a non-empty token");

        // Seed a HUD-originated input item directly on the authority (the
        // operator-input producer side is out of scope for this ingress test).
        let now = now_wall_us().max(1);
        driver
            .authority_mut()
            .enqueue_input(
                proj,
                "input-1",
                "hello from the HUD".to_string(),
                now,
                now + 60_000_000,
                Some(ContentClassification::Household),
            )
            .expect("enqueue_input must succeed for an attached projection");

        // 2. GetPendingInput via the op channel — returns the delivered item.
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::GetPendingInput {
            projection_id: proj.to_string(),
            owner_token: token.clone(),
            max_items: None,
            max_bytes: None,
            reply: tx,
        });
        let batch = rx
            .blocking_recv()
            .expect("get_pending_input reply must arrive")
            .expect("get_pending_input must be accepted");
        assert_eq!(batch.items.len(), 1, "exactly one pending item delivered");
        let item = &batch.items[0];
        assert_eq!(item.input_id, "input-1");
        assert_eq!(item.projection_id, proj);
        assert_eq!(item.submission_text, "hello from the HUD");
        assert_eq!(
            item.delivery_state, "delivered",
            "item must be transitioned to delivered by the poll"
        );
        assert_eq!(
            item.content_classification, "household",
            "classification must be mapped to its snake_case wire spelling"
        );
        assert_eq!(batch.remaining_count, 0);

        // Auth gate: a wrong token must be rejected (security regression guard).
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::GetPendingInput {
            projection_id: proj.to_string(),
            owner_token: "not-the-real-token".to_string(),
            max_items: None,
            max_bytes: None,
            reply: tx,
        });
        let denied = rx
            .blocking_recv()
            .expect("get_pending_input reply must arrive");
        assert!(
            denied.is_err(),
            "get_pending_input with a bad owner token must be denied"
        );

        // 3. AcknowledgeInput (handled) via the op channel.
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::AcknowledgeInput {
            projection_id: proj.to_string(),
            owner_token: token.clone(),
            input_id: "input-1".to_string(),
            ack_state: "handled".to_string(),
            ack_message: Some("done".to_string()),
            not_before_wall_us: None,
            reply: tx,
        });
        rx.blocking_recv()
            .expect("acknowledge reply must arrive")
            .expect("acknowledge_input (handled) must be accepted");

        // After a terminal ack, the item is no longer pending.
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::GetPendingInput {
            projection_id: proj.to_string(),
            owner_token: token.clone(),
            max_items: None,
            max_bytes: None,
            reply: tx,
        });
        let batch = rx
            .blocking_recv()
            .expect("get_pending_input reply must arrive")
            .expect("get_pending_input must be accepted");
        assert!(
            batch.items.is_empty(),
            "no pending input should remain after a terminal acknowledgement"
        );

        // Unrecognized ack_state is rejected before the authority is called.
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::AcknowledgeInput {
            projection_id: proj.to_string(),
            owner_token: token.clone(),
            input_id: "input-1".to_string(),
            ack_state: "bogus".to_string(),
            ack_message: None,
            not_before_wall_us: None,
            reply: tx,
        });
        let bad_ack = rx.blocking_recv().expect("acknowledge reply must arrive");
        assert!(
            bad_ack.is_err(),
            "an unrecognized ack_state must be rejected"
        );

        // 4. Detach via the op channel — purges authority + drive state.
        assert!(
            driver.authority_mut().has_projection(proj),
            "projection must exist before detach"
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::Detach {
            projection_id: proj.to_string(),
            owner_token: token.clone(),
            reason: "session ending".to_string(),
            reply: tx,
        });
        rx.blocking_recv()
            .expect("detach reply must arrive")
            .expect("detach must be accepted");
        assert!(
            !driver.authority_mut().has_projection(proj),
            "projection state must be purged after detach"
        );

        // Post-detach, a publish with the old token must fail (session gone).
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: proj.to_string(),
            owner_token: token,
            output_text: "after detach".to_string(),
            logical_unit_id: None,
            output_kind: None,
            content_classification: None,
            coalesce_key: None,
            expects_reply: None,
            reply: tx,
        });
        let after = rx.blocking_recv().expect("publish reply must arrive");
        assert!(
            after.is_err(),
            "publishing to a detached projection must be denied"
        );
    }

    /// End-to-end `publish_status` round-trip through `dispatch_portal_op`
    /// (hud-y8h3m). This is the production MCP ingress path for step 3 of the
    /// cooperative workflow: the LLM signals its lifecycle state to the viewer.
    ///
    /// Drives Attach → PublishStatus(active) → PublishStatus(degraded) and
    /// asserts that (a) the authority's session lifecycle state is updated, (b)
    /// the applied state is echoed back through the reply channel (the
    /// observable round-trip), (c) an unrecognized lifecycle string is rejected
    /// before the authority is touched, and (d) the owner-token auth gate holds.
    #[test]
    fn dispatch_portal_op_publish_status_round_trip() {
        let mut driver = InProcessPortalDriver::new();
        let proj = "proj-status";

        // Attach to obtain the owner token.
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: proj.to_string(),
            display_name: "Status Test".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: tx,
        });
        let token = rx
            .blocking_recv()
            .expect("attach reply must arrive")
            .expect("attach must be accepted");

        // A fresh attach (no publish traffic yet) is in the `Attached` state.
        assert_eq!(
            driver
                .authority_mut()
                .state_summary(proj)
                .expect("session must exist")
                .lifecycle_state,
            ProjectionLifecycleState::Attached,
        );

        // PublishStatus(active) — accepted, echoes the applied state back.
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::PublishStatus {
            projection_id: proj.to_string(),
            owner_token: token.clone(),
            lifecycle_state: "active".to_string(),
            status_text: Some("working".to_string()),
            reply: tx,
        });
        let applied = rx
            .blocking_recv()
            .expect("publish_status reply must arrive")
            .expect("publish_status must be accepted");
        assert_eq!(
            applied, "active",
            "the applied lifecycle state must round-trip back as snake_case"
        );
        assert_eq!(
            driver
                .authority_mut()
                .state_summary(proj)
                .expect("session must exist")
                .lifecycle_state,
            ProjectionLifecycleState::Active,
        );

        // PublishStatus(degraded) — the "blocked" signal; updates the session.
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::PublishStatus {
            projection_id: proj.to_string(),
            owner_token: token.clone(),
            lifecycle_state: "degraded".to_string(),
            status_text: None,
            reply: tx,
        });
        let applied = rx
            .blocking_recv()
            .expect("publish_status reply must arrive")
            .expect("publish_status must be accepted");
        assert_eq!(applied, "degraded");
        assert_eq!(
            driver
                .authority_mut()
                .state_summary(proj)
                .expect("session must exist")
                .lifecycle_state,
            ProjectionLifecycleState::Degraded,
            "the authority must hold the newly published lifecycle state",
        );

        // An unrecognized lifecycle string is rejected before the authority is
        // touched — the session keeps its prior (Degraded) state.
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::PublishStatus {
            projection_id: proj.to_string(),
            owner_token: token.clone(),
            lifecycle_state: "bogus".to_string(),
            status_text: None,
            reply: tx,
        });
        let rejected = rx
            .blocking_recv()
            .expect("publish_status reply must arrive");
        let rejection = rejected.expect_err("an unrecognized lifecycle_state must be rejected");
        assert_eq!(
            rejection.error_code,
            ProjectionErrorCode::ProjectionInvalidArgument,
        );
        assert_eq!(
            driver
                .authority_mut()
                .state_summary(proj)
                .expect("session must exist")
                .lifecycle_state,
            ProjectionLifecycleState::Degraded,
            "a rejected status must not mutate the session lifecycle state",
        );

        // Auth gate: a wrong owner token must be denied.
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::PublishStatus {
            projection_id: proj.to_string(),
            owner_token: "not-the-real-token".to_string(),
            lifecycle_state: "active".to_string(),
            status_text: None,
            reply: tx,
        });
        let denied = rx
            .blocking_recv()
            .expect("publish_status reply must arrive");
        assert!(
            denied.is_err(),
            "publish_status with a bad owner token must be denied"
        );
    }

    /// Capture an owner token via the production `dispatch_portal_op` Attach
    /// ingress (the path that wires the reconnect-clear hook), rather than calling
    /// the authority directly.
    fn dispatch_attach_and_get_token(driver: &mut InProcessPortalDriver, proj: &str) -> String {
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: proj.to_string(),
            display_name: format!("Test {proj}"),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: tx,
        });
        rx.blocking_recv()
            .expect("attach reply must arrive")
            .expect("attach must be accepted")
    }

    /// Publish through the production `dispatch_portal_op` ingress (the path that
    /// wires the reconnect-clear hook).
    fn dispatch_publish(driver: &mut InProcessPortalDriver, proj: &str, token: &str, text: &str) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: proj.to_string(),
            owner_token: token.to_string(),
            output_text: text.to_string(),
            logical_unit_id: None,
            output_kind: None,
            content_classification: None,
            coalesce_key: None,
            expects_reply: None,
            reply: tx,
        });
        rx.blocking_recv()
            .expect("publish reply must arrive")
            .expect("publish must be accepted");
    }

    fn connection_degraded(driver: &mut InProcessPortalDriver, proj: &str) -> Option<bool> {
        driver
            .authority_mut()
            .projected_portal_state(proj, &ProjectedPortalPolicy::permit_all())
            .map(|state| state.connection_degraded)
    }

    /// hud-5i16d: the dormant disconnect/degraded failure-UX is now wired into the
    /// driver. A headless drive of drop → degraded → reconnect → clear, plus the
    /// negative cases (clean detach never degrades; the latch is idempotent; a
    /// normal publish does not degrade).
    #[test]
    fn ungraceful_drop_degrades_and_reconnect_clears() {
        let mut driver = InProcessPortalDriver::new();
        let proj = "proj-disconnect";
        let token = dispatch_attach_and_get_token(&mut driver, proj);

        // A freshly-attached, never-disconnected portal is "connecting", not
        // degraded.
        assert_eq!(
            connection_degraded(&mut driver, proj),
            Some(false),
            "a fresh attach must not present as degraded"
        );

        // A normal publish (no prior drop) must not flip the surface to degraded
        // and must not perturb the connection latch.
        dispatch_publish(&mut driver, proj, &token, "live line");
        assert_eq!(
            connection_degraded(&mut driver, proj),
            Some(false),
            "a normal publish must not degrade the surface"
        );

        // (1) Ungraceful drop flips connection_degraded true.
        assert!(
            driver.mark_projection_disconnected_at(proj, 5_000),
            "the first drop must flip a live projection to disconnected"
        );
        assert_eq!(
            connection_degraded(&mut driver, proj),
            Some(true),
            "an ungraceful drop must make the next projected_portal_state degraded"
        );
        // The drive entry and tile mapping are RETAINED (degraded ≠ detach): the
        // retained transcript window stays on screen under the degraded treatment.
        assert!(
            driver.drive.entries.contains_key(proj),
            "an ungraceful drop must retain the drive entry (not detach the surface)"
        );

        // The latch is idempotent — a repeated drop signal is a no-op.
        assert!(
            !driver.mark_projection_disconnected_at(proj, 6_000),
            "a repeated drop on an already-disconnected projection must be a no-op"
        );
        assert_eq!(connection_degraded(&mut driver, proj), Some(true));

        // (3) Reconnect via the next publish clears the degraded treatment.
        dispatch_publish(&mut driver, proj, &token, "resumed line");
        assert_eq!(
            connection_degraded(&mut driver, proj),
            Some(false),
            "a publish after a drop must restore the connection and clear degraded"
        );

        // Drop again, then reconnect via a second publish (re-publish is the
        // reliable reconnect signal — a still-attached session keeps its owner
        // token and resumes publishing rather than re-attaching).
        assert!(driver.mark_projection_disconnected_at(proj, 7_000));
        assert_eq!(connection_degraded(&mut driver, proj), Some(true));
        dispatch_publish(&mut driver, proj, &token, "resumed again");
        assert_eq!(
            connection_degraded(&mut driver, proj),
            Some(false),
            "a second publish after a re-drop must clear the degraded treatment again"
        );
    }

    /// hud-5i16d: an idempotent re-attach replay (matching `idempotency_key`) is
    /// the attach-path reconnect signal and clears the degraded treatment. (A
    /// plain re-attach of a still-attached projection is rejected as
    /// already-attached, so only the idempotent replay exercises this hook.)
    #[test]
    fn idempotent_reattach_clears_degraded() {
        let mut driver = InProcessPortalDriver::new();
        let proj = "proj-reattach-clear";
        let key = "idem-key-1";

        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: proj.to_string(),
            display_name: "Reattach Clear".to_string(),
            idempotency_key: Some(key.to_string()),
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: tx,
        });
        rx.blocking_recv()
            .expect("attach reply must arrive")
            .expect("attach must be accepted");

        assert!(driver.mark_projection_disconnected_at(proj, 5_000));
        assert_eq!(connection_degraded(&mut driver, proj), Some(true));

        // Idempotent replay (same projection_id + idempotency_key) is accepted
        // and runs the reconnect-clear hook.
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: proj.to_string(),
            display_name: "Reattach Clear".to_string(),
            idempotency_key: Some(key.to_string()),
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: tx,
        });
        rx.blocking_recv()
            .expect("attach replay reply must arrive")
            .expect("idempotent attach replay must be accepted");

        assert_eq!(
            connection_degraded(&mut driver, proj),
            Some(false),
            "an idempotent re-attach replay after a drop must clear the degraded treatment"
        );
    }

    /// hud-5i16d: (2) a clean detach must NOT trigger degraded, and a late drop
    /// signal must not resurrect a degraded surface for a detached projection.
    #[test]
    fn clean_detach_does_not_degrade() {
        let mut driver = InProcessPortalDriver::new();
        let proj = "proj-clean-detach";
        let token = dispatch_attach_and_get_token(&mut driver, proj);
        dispatch_publish(&mut driver, proj, &token, "line before detach");

        // Clean detach through the production ingress.
        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::Detach {
            projection_id: proj.to_string(),
            owner_token: token.clone(),
            reason: "clean shutdown".to_string(),
            reply: tx,
        });
        rx.blocking_recv()
            .expect("detach reply must arrive")
            .expect("clean detach must be accepted");

        // The session is purged — there is no degraded surface to present.
        assert_eq!(
            connection_degraded(&mut driver, proj),
            None,
            "a clean detach purges the session — no degraded surface remains"
        );
        // A late drop signal must not resurrect a degraded surface: the entry is
        // already gone, so the latch reports nothing flipped.
        assert!(
            !driver.mark_projection_disconnected_at(proj, 9_000),
            "a drop signal after a clean detach must not resurrect a degraded surface"
        );
        assert_eq!(connection_degraded(&mut driver, proj), None);
    }

    /// hud-5i16d: the channel-close production path — `mark_all_projections_disconnected`
    /// latches every still-attached projection to degraded (the MCP `portal_op`
    /// channel disconnecting without per-projection clean Detach ops).
    #[test]
    fn mark_all_projections_disconnected_degrades_every_attached_portal() {
        let mut driver = InProcessPortalDriver::new();
        let proj_a = "proj-all-a";
        let proj_b = "proj-all-b";
        let _ = dispatch_attach_and_get_token(&mut driver, proj_a);
        let _ = dispatch_attach_and_get_token(&mut driver, proj_b);

        driver.mark_all_projections_disconnected();

        assert_eq!(
            connection_degraded(&mut driver, proj_a),
            Some(true),
            "channel close must degrade the first attached portal"
        );
        assert_eq!(
            connection_degraded(&mut driver, proj_b),
            Some(true),
            "channel close must degrade the second attached portal"
        );
    }

    /// hud-h3mvo: a pure upstream drop (no subsequent publish) must visibly
    /// repaint the portal tile to its degraded treatment within one drain,
    /// rather than waiting for the next publish-driven render.
    ///
    /// Before the fix, `mark_projection_disconnected_at` latched
    /// `connection_degraded` but enqueued no coalescer update, so the
    /// round-robin due-loop in `drain_inner` never revisited the tile — the
    /// disconnect stayed invisible until the next publish. The forced
    /// degraded-repaint pass repaints the flagged entry once.
    #[test]
    fn pure_drop_forces_degraded_repaint_without_subsequent_publish() {
        let mut driver = InProcessPortalDriver::new();

        let proj = "proj-pure-drop";
        let token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Drain 1: publish + drain → tile created and painted with live colors.
        // This projection stays on the default in-process transport, so its
        // materialisation happens in the scene (not over the bridge) — hud-g7ool.
        publish(&mut driver, proj, &token, "live line", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);
        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .tile_scene_id
                .is_some(),
            "tile must exist after the first drain"
        );
        let version_after_live = scene.version;

        // Pure drop: latch disconnected with NO subsequent publish.
        assert!(
            driver.mark_projection_disconnected_at(proj, 9_000),
            "a pure drop must latch the entry disconnected"
        );
        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .needs_degraded_repaint,
            "the drop must flag the entry for a forced degraded repaint"
        );

        // Drain with no new publish: the post-due-loop pass must repaint the tile.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 10_000);

        // The scene was repainted — version advances only because the tile root
        // was replaced (a no-op drain would leave it unchanged).
        assert!(
            scene.version > version_after_live,
            "a pure-drop drain must repaint the tile (scene version must advance) \
             without waiting for a publish"
        );
        // One-shot: the flag is consumed so an idle degraded tile is not
        // re-rendered on every subsequent drain.
        assert!(
            !driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .needs_degraded_repaint,
            "the forced degraded repaint must clear its flag (one-shot, not every drain)"
        );

        // A second drain with nothing new must be a no-op (no further repaint).
        let version_after_degraded = scene.version;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 11_000);
        assert_eq!(
            scene.version, version_after_degraded,
            "an idle degraded tile must not be repainted again on the next drain"
        );
    }

    // ── Agent-activity cue autonomous quiesce (hud-kbm80) ────────────────────
    //
    // The ambient "⋯ writing" header line + "▍" streaming cursor are derived from
    // the newest transcript unit's appended-at vs the render `now`. The drive loop
    // only re-renders a portal on a fresh coalescer update, so on a fully idle
    // portal the cue would persist past its window and misrepresent ongoing
    // activity. These tests pin the spec-SHALL fix: a one-shot force-repaint
    // scheduled at the derivation deadline quiesces the cue with no external
    // traffic — for the in-process tiled path and the bridged tee (hud-vne15) —
    // one-shot (not re-fired every tick), and reset/extended by a fresh append.

    /// Bring `proj` to a LIVE (ever-connected, non-degraded) state so the
    /// activity cue can fire (`has_ever_connected` gate).
    fn record_live_connection(driver: &mut InProcessPortalDriver, proj: &str) {
        driver
            .authority_mut()
            .record_hud_connection(
                proj,
                HudConnectionMetadata {
                    connection_id: format!("conn-{proj}"),
                    authenticated_session_id: format!("sess-{proj}"),
                    granted_capabilities: vec!["create_tiles".to_string()],
                    connected_at_wall_us: 50,
                    last_reconnect_wall_us: 50,
                },
            )
            .expect("record_hud_connection must succeed for an attached projection");
    }

    /// All painted text in the portal subtree. Expanded native projections use
    /// a `SolidColor` frame root with token-styled text children; collapsed
    /// projections retain the single `TextMarkdown` card root.
    fn tile_markdown(scene: &SceneGraph, tile_id: SceneId) -> String {
        fn collect(scene: &SceneGraph, node_id: SceneId, output: &mut String) {
            let node = scene.nodes.get(&node_id).expect("node id must resolve");
            if let tze_hud_scene::NodeData::TextMarkdown(text) = &node.data {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&text.content);
            }
            for child in &node.children {
                collect(scene, *child, output);
            }
        }

        let root_id = scene
            .tiles
            .get(&tile_id)
            .expect("portal tile must exist in scene")
            .root_node
            .expect("portal tile must have a painted root node");
        let mut output = String::new();
        collect(scene, root_id, &mut output);
        assert!(!output.is_empty(), "portal subtree must paint text content");
        output
    }

    #[test]
    fn parked_portal_repaints_pending_count_at_input_expiry() {
        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("pending_input_expiry"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };
        let projection_id = "pending-expiry-proj";
        let now_us = now_wall_us();
        let submitted_at_us = now_us.saturating_add(1);
        let pending_at_us = now_us.saturating_add(2);
        let expires_at_us = now_us.saturating_add(1_000_000);
        let token = attach_and_get_token_at(&mut driver, projection_id, now_us);
        driver.attach_projection(projection_id, Vec::new());
        publish(
            &mut driver,
            projection_id,
            &token,
            "waiting",
            submitted_at_us,
        );

        let mut scene = SceneGraph::new(800.0, 600.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), submitted_at_us);
        let tile_id = driver.drive.entries[projection_id]
            .tile_scene_id
            .expect("initial drain creates portal tile");

        let feedback = driver.authority_mut().submit_portal_input(
            projection_id,
            PortalInputSubmission {
                input_id: "pending-expiry-input".to_string(),
                submission_text: "will expire".to_string(),
                submitted_at_wall_us: pending_at_us,
                expires_at_wall_us: Some(expires_at_us),
                content_classification: ContentClassification::Private,
            },
        );
        assert_eq!(feedback.feedback_state, PortalInputFeedbackState::Accepted);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), pending_at_us);
        let pending_markdown = tile_markdown(&scene, tile_id);
        assert!(
            pending_markdown.contains("pending HUD input: 1"),
            "accepted pending input must render its compact count: {pending_markdown}"
        );
        assert_eq!(
            driver.next_wake_deadline(&scene),
            Some(PortalWakeDeadline {
                wall_us: expires_at_us,
                family: PortalDeadlineFamily::PendingInputExpiry,
            })
        );

        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), expires_at_us);
        let summary = driver.authority_mut().state_summary(projection_id).unwrap();
        assert_eq!(summary.pending_input_count, 0);
        assert_eq!(summary.pending_input_bytes, 0);
        let expired_markdown = tile_markdown(&scene, tile_id);
        assert!(
            expired_markdown.contains("pending HUD input: 0"),
            "the expiry drain must repaint the compact pending count: {expired_markdown}"
        );
        assert_ne!(
            driver
                .next_wake_deadline(&scene)
                .map(|deadline| deadline.family),
            Some(PortalDeadlineFamily::PendingInputExpiry),
            "expiry deadline must be one-shot after the parked transition"
        );
    }

    #[test]
    fn portal_deadline_families_preserve_source_and_equal_deadline_tie() {
        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                agent_liveness_degraded_after_wall_us: 10_000_000,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("deadline_families"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };
        let projection_id = "deadline-family-proj";
        let now_us = now_wall_us();
        let token = attach_and_get_token_at(&mut driver, projection_id, now_us);
        driver.attach_projection(projection_id, Vec::new());
        publish(&mut driver, projection_id, &token, "first", now_us);
        let mut scene = SceneGraph::new(800.0, 600.0);

        assert_eq!(
            driver.next_wake_deadline(&scene).unwrap().family,
            PortalDeadlineFamily::Cadence
        );

        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), now_us);
        driver
            .drive
            .entries
            .get_mut(projection_id)
            .unwrap()
            .activity_cue_clear_due_us = Some(now_us.saturating_add(500));
        assert_eq!(
            driver.next_wake_deadline(&scene),
            Some(PortalWakeDeadline {
                wall_us: now_us.saturating_add(500),
                family: PortalDeadlineFamily::ActivityCue,
            })
        );

        driver
            .drive
            .entries
            .get_mut(projection_id)
            .unwrap()
            .activity_cue_clear_due_us = None;
        publish(
            &mut driver,
            projection_id,
            &token,
            "second",
            now_us.saturating_add(100),
        );
        let cadence_deadline = driver
            .next_wake_deadline(&scene)
            .expect("pending publish schedules cadence");
        assert_eq!(cadence_deadline.family, PortalDeadlineFamily::Cadence);
        driver
            .drive
            .entries
            .get_mut(projection_id)
            .unwrap()
            .activity_cue_clear_due_us = Some(cadence_deadline.wall_us);
        assert_eq!(
            driver.next_wake_deadline(&scene).unwrap().family,
            PortalDeadlineFamily::Cadence,
            "equal cadence/activity deadlines use the declared family order"
        );
        assert!(
            [
                (
                    PortalDeadlineFamily::ImmediateWork,
                    RuntimeWakeupSource::SceneChange,
                ),
                (
                    PortalDeadlineFamily::Cadence,
                    RuntimeWakeupSource::AnimationDeadline,
                ),
                (
                    PortalDeadlineFamily::ActivityCue,
                    RuntimeWakeupSource::AnimationDeadline,
                ),
                (
                    PortalDeadlineFamily::AgentLiveness,
                    RuntimeWakeupSource::TtlDeadline,
                ),
                (
                    PortalDeadlineFamily::PendingInputExpiry,
                    RuntimeWakeupSource::TtlDeadline,
                ),
                (
                    PortalDeadlineFamily::LeaseLifecycle,
                    RuntimeWakeupSource::TtlDeadline,
                ),
            ]
            .into_iter()
            .all(|(family, source)| family.wakeup_source() == source),
            "every portal deadline family must retain its counter attribution"
        );
    }

    #[test]
    fn cleanup_and_drop_work_remain_immediate_after_a_simulated_lock_miss() {
        let mut driver = InProcessPortalDriver::new();
        let projection_id = "retry-cleanup-proj";
        let token = attach_and_get_token(&mut driver, projection_id);
        driver.attach_projection(projection_id, Vec::new());
        publish(&mut driver, projection_id, &token, "surface", 100);
        let mut scene = SceneGraph::new(800.0, 600.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 100);
        assert_eq!(scene.tile_count(), 1);

        driver.detach_projection(projection_id);
        // No drain here models the about_to_wait turn whose scene try-lock
        // missed. The scheduler must still discover immediate cleanup work.
        assert!(driver.has_immediate_work(&scene));
        assert_eq!(
            driver.next_wake_deadline(&scene).unwrap().family,
            PortalDeadlineFamily::ImmediateWork
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 101);
        assert_eq!(scene.tile_count(), 0);
        assert!(!driver.has_immediate_work(&scene));

        let projection_id = "retry-drop-proj";
        let token = attach_and_get_token(&mut driver, projection_id);
        driver.attach_projection(projection_id, Vec::new());
        publish(&mut driver, projection_id, &token, "live surface", 200);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);
        let lease_id = driver.drive.entries[projection_id]
            .scene_lease_id
            .expect("drop test lease");
        driver.mark_all_projections_disconnected();
        assert!(driver.has_immediate_work(&scene));
        assert_eq!(
            driver.next_wake_deadline(&scene).unwrap().family,
            PortalDeadlineFamily::ImmediateWork
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 201);
        assert!(scene.lease_is_orphaned(&lease_id));
        assert!(
            driver
                .authority_mut()
                .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
                .unwrap()
                .connection_degraded
        );
        assert!(!driver.drive.entries[projection_id].needs_degraded_repaint);
    }

    /// A projection can be attached before it has published any content, so it
    /// has no in-process tile to repaint. Its liveness transition still needs
    /// one authority sweep, but must not leave inert repaint debt that schedules
    /// `ImmediateWork` at roughly 1ms forever.
    #[test]
    fn attach_only_liveness_degradation_consumes_unmaterializable_repaint_debt() {
        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                agent_liveness_degraded_after_wall_us: 1_000_000,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("attach_only_liveness"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };
        let projection_id = "attach-only-liveness";
        let attached_at_us = 1_000;
        let _token = attach_and_get_token_at(&mut driver, projection_id, attached_at_us);
        driver.attach_projection(projection_id, Vec::new());

        let mut scene = SceneGraph::new(800.0, 600.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        let liveness_deadline_us = attached_at_us.saturating_add(1_000_000);
        assert_eq!(
            driver.next_wake_deadline(&scene),
            Some(PortalWakeDeadline {
                wall_us: liveness_deadline_us,
                family: PortalDeadlineFamily::AgentLiveness,
            })
        );

        driver.drain_inner(
            &mut scene,
            &mut processor,
            Some(tab_id),
            liveness_deadline_us,
        );

        assert!(driver.authority.is_agent_liveness_degraded(projection_id));
        let entry = &driver.drive.entries[projection_id];
        assert!(
            entry.tile_scene_id.is_none(),
            "attach-only projections have no in-process surface to repaint"
        );
        assert!(
            !entry.needs_degraded_repaint,
            "a tile-less in-process projection must consume repaint debt after the liveness sweep"
        );
        assert_eq!(
            driver.next_wake_deadline(&scene),
            None,
            "no tile, lease, or timed portal work remains after the one-shot liveness transition"
        );
    }

    /// A channel-drop path must retain its first immediate drain so real work is
    /// not delayed, then consume the repaint flag when the attached projection
    /// has no in-process tile to materialise.
    #[test]
    fn attach_only_drop_consumes_unmaterializable_repaint_debt_after_one_drain() {
        let mut driver = InProcessPortalDriver::new();
        let projection_id = "attach-only-drop";
        let _token = attach_and_get_token(&mut driver, projection_id);
        driver.attach_projection(projection_id, Vec::new());

        let mut scene = SceneGraph::new(800.0, 600.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        assert!(driver.mark_projection_disconnected_at(projection_id, 2_000));
        assert_eq!(
            driver
                .next_wake_deadline(&scene)
                .map(|deadline| deadline.family),
            Some(PortalDeadlineFamily::ImmediateWork),
            "the initial drop still receives an immediate authoritative drain"
        );

        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 2_000);

        let entry = &driver.drive.entries[projection_id];
        assert!(entry.tile_scene_id.is_none());
        assert!(
            !entry.needs_degraded_repaint,
            "the first drain must consume drop repaint debt that has no tile to paint"
        );
        assert_eq!(
            driver.next_wake_deadline(&scene),
            None,
            "the dropped attach-only projection must not schedule repeated ~1ms ImmediateWork wakes"
        );
    }

    /// A fully-idle in-process portal must quiesce its activity cue on its own:
    /// after the terminal append, a one-shot force-repaint past the quiesce
    /// window clears the "⋯ writing" header + streaming cursor with no new update.
    #[test]
    fn activity_cue_quiesces_after_deadline_without_new_update() {
        let mut driver = InProcessPortalDriver::new();
        let proj = "proj-cue-quiesce";
        let token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());
        record_live_connection(&mut driver, proj);

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Publish a fresh agent turn; drain within the quiesce window so the tile
        // paints the ambient activity cue.
        publish(&mut driver, proj, &token, "streaming line", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let tile_id = driver
            .drive
            .entries
            .get(proj)
            .unwrap()
            .tile_scene_id
            .expect("first drain must create the portal tile");
        assert!(
            tile_markdown(&scene, tile_id).contains(PORTAL_ACTIVITY_MARKER_TEXT),
            "the activity cue must be painted while the agent tail is fresh"
        );

        // The materialisation scheduled a one-shot cue-quiesce repaint.
        let due = driver
            .drive
            .entries
            .get(proj)
            .unwrap()
            .activity_cue_clear_due_us
            .expect("a live fresh agent tail must schedule a cue-quiesce repaint");

        // Drain at/after the deadline with NO new update: the forced pass repaints
        // once, re-running the (now-false) cue derivation → cue cleared.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), due);
        assert!(
            !tile_markdown(&scene, tile_id).contains(PORTAL_ACTIVITY_MARKER_TEXT),
            "the cue SHALL quiesce promptly once appends stop, even on an idle portal \
             (portal-chat-grade-affordances §Agent Activity and Streaming Cue)"
        );
        // One-shot: the deadline is consumed.
        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .activity_cue_clear_due_us
                .is_none(),
            "the forced quiesce repaint must clear its one-shot deadline"
        );

        // A further idle drain must not repaint again (one-shot, not every tick).
        let version_after_quiesce = scene.version;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), due + 1_000_000);
        assert_eq!(
            scene.version, version_after_quiesce,
            "a quiesced idle portal must not be repainted on every subsequent drain"
        );
    }

    /// A fresh append before the deadline must reset/extend it: the earlier
    /// scheduled quiesce must not fire while the agent is still streaming.
    #[test]
    fn fresh_append_extends_activity_cue_deadline() {
        let mut driver = InProcessPortalDriver::new();
        let proj = "proj-cue-extend";
        let token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());
        record_live_connection(&mut driver, proj);

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // First append + drain schedules the initial deadline.
        publish(&mut driver, proj, &token, "line one", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);
        let tile_id = driver
            .drive
            .entries
            .get(proj)
            .unwrap()
            .tile_scene_id
            .unwrap();
        let due1 = driver
            .drive
            .entries
            .get(proj)
            .unwrap()
            .activity_cue_clear_due_us
            .expect("first fresh tail schedules a deadline");

        // A fresh append past the rate window re-materialises and must push the
        // deadline forward (the cue keeps streaming, not quiescing).
        let ts_two = PORTAL_UPDATE_RATE_WINDOW_WALL_US;
        let drain_two_now = PORTAL_UPDATE_RATE_WINDOW_WALL_US * 2;
        publish(&mut driver, proj, &token, "line two", ts_two);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), drain_two_now);
        let due2 = driver
            .drive
            .entries
            .get(proj)
            .unwrap()
            .activity_cue_clear_due_us
            .expect("a fresh append re-schedules the deadline");
        assert!(
            due2 > due1,
            "a fresh append must extend the cue-quiesce deadline (due2 {due2} > due1 {due1})"
        );

        // Draining at the ORIGINAL deadline must NOT quiesce: the fresh append
        // moved the deadline out, so the cue is still live and stays painted.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), due1);
        assert!(
            tile_markdown(&scene, tile_id).contains(PORTAL_ACTIVITY_MARKER_TEXT),
            "the cue must remain while a fresh append keeps the tail within the window"
        );
        assert_eq!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .activity_cue_clear_due_us,
            Some(due2),
            "the extended deadline must be retained (the old one must not have fired)"
        );
    }

    /// The bridged tee path must also quiesce (hud-vne15): a bridged projection
    /// has no in-process tile, so the pass forwards a fresh `Publish` at the
    /// deadline carrying the now-quiesced state, one-shot.
    #[test]
    fn bridged_activity_cue_quiesce_forwarded_to_bridge() {
        let mut driver = InProcessPortalDriver::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeMessage>(16);
        driver.set_resident_grpc_bridge_tx(Some(tx));

        let proj = "proj-bridged-cue";
        let token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());
        driver.set_projection_transport(proj, PortalTransport::ResidentGrpcBridge);
        record_live_connection(&mut driver, proj);

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Materialise once over the bridge (live), drain that Publish so the
        // channel only carries post-deadline traffic below.
        publish(&mut driver, proj, &token, "bridged streaming", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);
        while rx.try_recv().is_ok() {}

        // A bridged projection has no in-process tile, yet the live materialisation
        // scheduled the one-shot cue-quiesce deadline.
        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .tile_scene_id
                .is_none(),
            "a bridged projection must have no in-process tile"
        );
        let due = driver
            .drive
            .entries
            .get(proj)
            .unwrap()
            .activity_cue_clear_due_us
            .expect("a bridged live fresh tail must schedule a cue-quiesce repaint");

        // Drain at the deadline with no new publish: the pass must forward exactly
        // one Publish (the bridge re-materialises the quiesced state remotely).
        let version_before = scene.version;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), due);

        let mut quiesce_publishes = 0;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                BridgeMessage::Publish { projection_id, .. } => {
                    assert_eq!(projection_id, proj, "unexpected projection id in tee");
                    quiesce_publishes += 1;
                }
                other => panic!("unexpected bridge message on a cue-quiesce drain: {other:?}"),
            }
        }
        assert_eq!(
            quiesce_publishes, 1,
            "a bridged cue quiesce must forward exactly one Publish to the bridge"
        );
        assert_eq!(
            scene.version, version_before,
            "a bridged cue quiesce must not mutate the in-process scene"
        );

        // One-shot: the deadline is consumed; a further idle drain tees nothing.
        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .activity_cue_clear_due_us
                .is_none(),
            "the forwarded quiesce state must clear the one-shot deadline"
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), due + 1_000_000);
        assert!(
            rx.try_recv().is_err(),
            "an idle quiesced bridged entry must not be re-teed on the next drain"
        );
    }

    /// hud-kbm80 (review follow-up): the cue-quiesce repaint must not drop the
    /// ambient "N unread" indicator. `take_due_portal_update` zeroes the session's
    /// unread count, and the quiesce pass re-derives state from that zeroed session
    /// — so without carrying the last-materialised count forward it would repaint
    /// the unread indicator away even though no viewer action cleared it. A fresh
    /// agent turn is itself unread output, so this overlap (cue active AND unread
    /// > 0) is the common case, not an edge one.
    #[test]
    fn activity_cue_quiesce_preserves_unread_indicator() {
        let mut driver = InProcessPortalDriver::new();
        let proj = "proj-cue-unread";
        let token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());
        record_live_connection(&mut driver, proj);

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // A single fresh agent turn: the cue is active AND the output is unread.
        publish(&mut driver, proj, &token, "streaming line", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        let tile_id = driver
            .drive
            .entries
            .get(proj)
            .unwrap()
            .tile_scene_id
            .expect("first drain must create the portal tile");
        let painted = tile_markdown(&scene, tile_id);
        assert!(
            painted.contains(PORTAL_ACTIVITY_MARKER_TEXT),
            "the activity cue must be painted while the agent tail is fresh"
        );
        assert!(
            painted.contains("1 unread"),
            "the ambient unread indicator must paint the live count on the drain render"
        );

        let due = driver
            .drive
            .entries
            .get(proj)
            .unwrap()
            .activity_cue_clear_due_us
            .expect("a live fresh agent tail must schedule a cue-quiesce repaint");

        // Quiesce with NO new update and NO viewer action: the cue clears, but the
        // unread indicator must survive (the count was not cleared by a viewer).
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), due);
        let quiesced = tile_markdown(&scene, tile_id);
        assert!(
            !quiesced.contains(PORTAL_ACTIVITY_MARKER_TEXT),
            "the cue SHALL quiesce once appends stop"
        );
        assert!(
            quiesced.contains("1 unread"),
            "the cue-quiesce repaint must NOT drop the ambient unread indicator \
             (hud-kbm80 review follow-up): no viewer action cleared it"
        );
    }

    /// hud-kbm80 (review follow-up): a bridged cue-quiesce `Publish` has no later
    /// update to supersede it, so it must survive bounded-channel backpressure.
    /// When `try_send` returns `Full`, the one-shot deadline must be RETAINED and
    /// the next drain must retry, rather than clearing the deadline and stranding
    /// the remote portal's writing cue forever.
    #[test]
    fn bridged_cue_quiesce_retries_when_bridge_channel_full() {
        let mut driver = InProcessPortalDriver::new();
        // Capacity-1 channel so a single un-drained message wedges `try_send`.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeMessage>(1);
        driver.set_resident_grpc_bridge_tx(Some(tx));

        let proj = "proj-bridged-cue-full";
        let token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());
        driver.set_projection_transport(proj, PortalTransport::ResidentGrpcBridge);
        record_live_connection(&mut driver, proj);

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Live materialisation fills the capacity-1 channel (1/1) and schedules the
        // deadline. Leave that Publish UN-drained so the channel stays full.
        publish(&mut driver, proj, &token, "bridged streaming", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);
        let due = driver
            .drive
            .entries
            .get(proj)
            .unwrap()
            .activity_cue_clear_due_us
            .expect("a bridged live fresh tail must schedule a cue-quiesce repaint");

        // Drain at the deadline while the channel is still full: the quiesce Publish
        // cannot be enqueued, so the deadline must be retained for a retry.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), due);
        assert_eq!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .activity_cue_clear_due_us,
            Some(due),
            "a Full bridge channel must NOT consume the one-shot cue-quiesce deadline"
        );

        // Free the channel (drain the stale live Publish), then drain again: the
        // retry now succeeds, forwarding exactly one quiesce Publish and clearing
        // the deadline.
        assert!(
            matches!(rx.try_recv(), Ok(BridgeMessage::Publish { .. })),
            "the live materialisation Publish must be the message wedging the channel"
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), due);
        assert!(
            matches!(rx.try_recv(), Ok(BridgeMessage::Publish { .. })),
            "the retry must forward the quiesce Publish once the channel drains"
        );
        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .activity_cue_clear_due_us
                .is_none(),
            "a successful retry must clear the one-shot deadline"
        );
    }

    // ── Per-projection transport selection (hud-g7ool) ───────────────────────
    //
    // v1 routing policy (owner decision, OPTION B): each projection is
    // materialised by EXACTLY ONE transport (bridge XOR in-process). These three
    // tests pin the acceptance criteria: (a) a bridged projection materialises
    // once via the bridge with its in-process direct path suppressed; (b) a
    // non-bridged projection still materialises via the direct path and is never
    // teed; (c) detaching a bridged projection emits a Detach tombstone so the
    // bridge tears down the remote portal (absorbs hud-sjdkk).

    /// (a) A projection routed to the bridge is materialised SOLELY over the
    /// bridge: no in-process scene tile, one tile-less governance lease, exactly
    /// one `Publish`.
    #[test]
    fn bridged_projection_materialises_via_bridge_and_suppresses_direct_path() {
        let mut driver = InProcessPortalDriver::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeMessage>(16);
        driver.set_resident_grpc_bridge_tx(Some(tx));

        let proj = "proj-bridged";
        let token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());
        driver.set_projection_transport(proj, PortalTransport::ResidentGrpcBridge);

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        let version_before = scene.version;

        publish(&mut driver, proj, &token, "bridged line", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        // In-process direct path suppressed: no scene tile. The sole scene
        // mutation is the tile-less lease used for stale-surface governance.
        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .tile_scene_id
                .is_none(),
            "a bridged projection must NOT create an in-process scene tile"
        );
        assert_eq!(
            scene.version,
            version_before + 1,
            "a bridged projection creates only its tile-less governance lease"
        );

        // The bridge is the sole materialiser: exactly one Publish for this proj.
        let mut publishes = 0;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                BridgeMessage::Publish { projection_id, .. } => {
                    assert_eq!(projection_id, proj, "unexpected projection id in tee");
                    publishes += 1;
                }
                other => panic!("unexpected bridge message on a live publish: {other:?}"),
            }
        }
        assert_eq!(
            publishes, 1,
            "a bridged projection must materialise exactly once via the bridge"
        );
    }

    /// (b) A non-bridged projection (the default) still materialises via the
    /// in-process direct path and is never teed to the bridge — even when a bridge
    /// channel is installed for other projections.
    #[test]
    fn non_bridged_projection_materialises_in_process_and_is_not_teed() {
        let mut driver = InProcessPortalDriver::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeMessage>(16);
        // Bridge channel installed, but this projection is left on the default
        // in-process transport (not routed to the bridge).
        driver.set_resident_grpc_bridge_tx(Some(tx));

        let proj = "proj-inproc";
        let token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        publish(&mut driver, proj, &token, "in-process line", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .tile_scene_id
                .is_some(),
            "a non-bridged projection must materialise via the in-process direct path"
        );
        assert!(
            rx.try_recv().is_err(),
            "a non-bridged projection must NOT be teed to the resident gRPC bridge"
        );
    }

    /// (c) Detaching a bridged projection emits a `Detach` tombstone so the bridge
    /// tears down the remote portal (no stale remote portal) — absorbs hud-sjdkk.
    #[test]
    fn detaching_a_bridged_projection_emits_a_detach_tombstone() {
        let mut driver = InProcessPortalDriver::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeMessage>(16);
        driver.set_resident_grpc_bridge_tx(Some(tx));

        let proj = "proj-bridged-detach";
        let token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());
        driver.set_projection_transport(proj, PortalTransport::ResidentGrpcBridge);

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Materialise once over the bridge, then drain that Publish from the tee.
        publish(&mut driver, proj, &token, "bridged line", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);
        while rx.try_recv().is_ok() {}

        // Detaching the bridged projection sends the teardown tombstone.
        driver.detach_projection(proj);

        let mut saw_detach = false;
        while let Ok(msg) = rx.try_recv() {
            if let BridgeMessage::Detach { projection_id } = msg {
                assert_eq!(
                    projection_id, proj,
                    "tombstone must target the detached proj"
                );
                saw_detach = true;
            }
        }
        assert!(
            saw_detach,
            "detaching a bridged projection must emit a Detach tombstone to the bridge"
        );
    }

    // ── Production routing wiring (hud-hfuxy) ─────────────────────────────────
    //
    // hud-g7ool installed the `PortalTransport` discriminant and the
    // `set_projection_transport` seam, but nothing in production ever called
    // it — every projection defaulted to `InProcess` forever and the bridge
    // stayed materialised-but-inert even when explicitly enabled. These two
    // tests drive the actual production entry point (`dispatch_portal_op`, the
    // same call `windowed/mod.rs::drain_portal_ops` makes) rather than calling
    // `set_projection_transport` directly, to pin the wiring itself.

    /// Attaching through `dispatch_portal_op` with the bridge channel already
    /// installed must route the new projection onto the bridge: exactly one
    /// `Publish`, no in-process tile, no scene mutation.
    #[test]
    fn dispatch_portal_op_attach_routes_to_bridge_when_installed() {
        let mut driver = InProcessPortalDriver::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeMessage>(16);
        driver.set_resident_grpc_bridge_tx(Some(tx));

        let proj = "proj-auto-bridge";
        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: proj.to_string(),
            display_name: "Test Projection".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let token = attach_rx
            .try_recv()
            .expect("reply must be sent synchronously")
            .expect("Attach must be accepted");

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        let version_before = scene.version;

        publish(&mut driver, proj, &token, "auto-bridged line", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .tile_scene_id
                .is_none(),
            "attaching through the production dispatch_portal_op path with the \
             bridge installed must route to the bridge (no in-process tile) — \
             this is the wiring hud-hfuxy adds"
        );
        assert_eq!(
            scene.version,
            version_before + 1,
            "a bridge-routed projection creates only its tile-less governance lease"
        );

        let mut publishes = 0;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                BridgeMessage::Publish { projection_id, .. } => {
                    assert_eq!(projection_id, proj, "unexpected projection id in tee");
                    publishes += 1;
                }
                other => panic!("unexpected bridge message on a live publish: {other:?}"),
            }
        }
        assert_eq!(publishes, 1, "must materialise exactly once via the bridge");
    }

    /// Attaching through `dispatch_portal_op` with no bridge channel installed
    /// (the default, shipped deployment) must leave the projection on the
    /// in-process path — byte-for-byte unchanged.
    #[test]
    fn dispatch_portal_op_attach_stays_in_process_when_bridge_not_installed() {
        let mut driver = InProcessPortalDriver::new();

        let proj = "proj-no-bridge";
        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: proj.to_string(),
            display_name: "Test Projection".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let token = attach_rx
            .try_recv()
            .expect("reply must be sent synchronously")
            .expect("Attach must be accepted");

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        publish(&mut driver, proj, &token, "default line", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);

        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .tile_scene_id
                .is_some(),
            "default deployment (bridge not installed) must be byte-for-byte \
             unchanged: projections still materialise in-process"
        );
    }

    /// hud-vne15: a pure upstream drop of a BRIDGED projection must forward its
    /// degraded state to the resident gRPC bridge, so the remote portal dims like
    /// an in-process portal would.
    ///
    /// A bridged projection has no in-process tile (`tile_scene_id.is_none()`), so
    /// before the fix it was excluded by the degraded-repaint pass's
    /// `tile_scene_id.is_some()` filter and its degraded state was never teed —
    /// the remote portal kept its live paint. The tile-OR-bridged filter now admits
    /// it and the pass forwards a `Publish` carrying `connection_degraded = true`.
    #[test]
    fn bridged_projection_pure_drop_forwards_degraded_state_to_bridge() {
        let mut driver = InProcessPortalDriver::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeMessage>(16);
        driver.set_resident_grpc_bridge_tx(Some(tx));

        let proj = "proj-bridged-drop";
        let token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());
        driver.set_projection_transport(proj, PortalTransport::ResidentGrpcBridge);

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Materialise once over the bridge (live), then drain that Publish so the
        // channel only carries post-drop traffic below.
        publish(&mut driver, proj, &token, "bridged line", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);
        while rx.try_recv().is_ok() {}

        // This is precisely the excluded case: a bridged projection has no
        // in-process tile, yet a pure drop flags it for a degraded repaint.
        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .tile_scene_id
                .is_none(),
            "a bridged projection must have no in-process tile"
        );
        assert!(
            driver.mark_projection_disconnected_at(proj, 9_000),
            "a pure drop must latch the bridged entry disconnected"
        );
        assert!(
            driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .needs_degraded_repaint,
            "the drop must flag the bridged entry for a forced degraded repaint"
        );

        let version_before_drop_drain = scene.version;

        // Drain with no new publish: the degraded-repaint pass must forward the
        // degraded state to the bridge (not repaint an absent in-process tile).
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 10_000);

        let mut degraded_publishes = 0;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                BridgeMessage::Publish {
                    projection_id,
                    state,
                } => {
                    assert_eq!(projection_id, proj, "unexpected projection id in tee");
                    assert!(
                        state.connection_degraded,
                        "the forwarded state must carry connection_degraded = true \
                         so the remote portal dims"
                    );
                    degraded_publishes += 1;
                }
                other => panic!("unexpected bridge message on a pure drop: {other:?}"),
            }
        }
        assert_eq!(
            degraded_publishes, 1,
            "a bridged pure drop must forward exactly one degraded Publish to the bridge"
        );

        // The bridged path paints no local tile. Its one scene mutation here is
        // orphaning the tile-less governance lease to start the shared grace
        // mechanism.
        assert_eq!(
            scene.version,
            version_before_drop_drain + 1,
            "a bridged degraded drain only orphans its tile-less governance lease"
        );

        // One-shot: the flag is consumed so an idle degraded bridged entry is not
        // re-teed on every subsequent drain.
        assert!(
            !driver
                .drive
                .entries
                .get(proj)
                .unwrap()
                .needs_degraded_repaint,
            "the forwarded degraded state must clear the one-shot flag"
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 11_000);
        assert!(
            rx.try_recv().is_err(),
            "an idle degraded bridged entry must not be re-teed on the next drain"
        );
    }

    #[test]
    fn dispatch_portal_op_operator_cleanup_purges_authority_and_drive_state() {
        let mut driver = InProcessPortalDriver::new();
        driver
            .authority_mut()
            .set_operator_authority("operator-secret")
            .expect("operator credential must configure");
        let proj = "proj-cleanup";

        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: proj.to_string(),
            display_name: "Cleanup Test".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: tx,
        });
        rx.blocking_recv()
            .expect("attach reply must arrive")
            .expect("attach must be accepted");
        assert!(
            driver.authority_mut().has_projection(proj),
            "projection must exist before cleanup"
        );
        assert!(
            driver.drive.entries.contains_key(proj),
            "dispatch attach must create a driver entry"
        );

        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::Cleanup {
            projection_id: proj.to_string(),
            cleanup_authority: "operator".to_string(),
            owner_token: None,
            operator_authority: Some("operator-secret".to_string()),
            reason: "operator override".to_string(),
            reply: tx,
        });
        rx.blocking_recv()
            .expect("cleanup reply must arrive")
            .expect("operator cleanup must be accepted");

        assert!(
            !driver.authority_mut().has_projection(proj),
            "operator cleanup must purge authority state"
        );
        assert!(
            !driver.drive.entries.contains_key(proj),
            "operator cleanup must drop the driver entry"
        );
    }

    #[test]
    fn dispatch_portal_op_operator_cleanup_removes_projection_tile_on_next_drain() {
        let mut driver = InProcessPortalDriver::new();
        driver
            .authority_mut()
            .set_operator_authority("operator-secret")
            .expect("operator credential must configure");
        let proj = "proj-cleanup-tile";
        let token = attach_and_get_token(&mut driver, proj);
        driver.attach_projection(proj, Vec::new());

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        publish(&mut driver, proj, &token, "line before cleanup", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);
        let tile_id = driver
            .drive
            .entries
            .get(proj)
            .expect("drive entry must exist")
            .tile_scene_id
            .expect("drain must create a portal tile");
        let lease_id = driver
            .drive
            .entries
            .get(proj)
            .and_then(|entry| entry.scene_lease_id)
            .expect("materialised projection must own a lease");
        assert!(
            scene.tiles.contains_key(&tile_id),
            "precondition: projected tile must exist before cleanup"
        );
        assert!(
            scene.tile_scroll_config(tile_id).is_some(),
            "precondition: projected tile must have scroll state before cleanup"
        );

        let (tx, rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op(PortalOp::Cleanup {
            projection_id: proj.to_string(),
            cleanup_authority: "operator".to_string(),
            owner_token: None,
            operator_authority: Some("operator-secret".to_string()),
            reason: "operator override".to_string(),
            reply: tx,
        });
        rx.blocking_recv()
            .expect("cleanup reply must arrive")
            .expect("operator cleanup must be accepted");
        assert!(
            !driver.drive.entries.contains_key(proj),
            "accepted cleanup must drop the drive entry immediately"
        );
        assert!(
            scene.tiles.contains_key(&tile_id),
            "tile removal is queued until the next drain has scene access"
        );

        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 300);

        assert!(
            !scene.tiles.contains_key(&tile_id),
            "accepted cleanup must remove the projection tile on the next drain"
        );
        assert!(
            scene.tile_scroll_config(tile_id).is_none(),
            "accepted cleanup must clear scene scroll config for the projection tile"
        );
        assert!(
            !scene.tile_follow_tail_at_tail(tile_id),
            "accepted cleanup must clear follow-tail scene state for the projection tile"
        );
        assert!(
            scene.drain_removed_tile_ids().contains(&tile_id),
            "accepted cleanup must publish a removed-tile notification for external state pruning"
        );
        assert!(
            !scene.lease_is_active(&lease_id),
            "accepted cleanup must revoke the detached projection's lease"
        );
    }

    /// hud-xlx1r (tasks 3.2 + AC#1): a portal left disconnected until its lease
    /// grace expires has its governed surface removed via the orphan path
    /// (`SceneGraph::expire_leases`) AND yields NO further `ProjectedPortalState`.
    ///
    /// This pins the *Stale-Content Degradation Contract* end of the lifecycle:
    /// the degraded window is bounded by lease grace, and once grace expires the
    /// surface is gone and the projection produces no state — so stale content can
    /// never be re-materialised after grace.
    ///
    /// Complements `reattach_after_grace_expiry_starts_fresh_portal_under_new_lease`
    /// (task 4.6, which proves a *new* session gets a fresh portal): here we track
    /// the *same* projection across live → degraded → grace-removed → silent.
    ///
    /// GAP NOTE: the production caller that drives the full liveness-gap →
    /// `mark_hud_disconnected` → scene orphan → `expire_projection` sequence is
    /// still dormant (sibling bead hud-5i16d wires the runtime trigger). This test
    /// invokes the individual steps directly to lock the scene+authority contract
    /// that wiring must satisfy; it does not exercise the (absent) production
    /// trigger.
    #[test]
    fn disconnected_portal_surface_removed_on_grace_expiry_yields_no_further_state() {
        use std::sync::Arc;
        use tze_hud_projection::HudConnectionMetadata;
        use tze_hud_scene::{Clock, TestClock};

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let projection_id = "proj-grace";
        let token = attach_and_get_token(&mut driver, projection_id);
        driver.attach_projection(projection_id, Vec::new());
        publish(
            &mut driver,
            projection_id,
            &token,
            "committed before drop",
            100,
        );

        // Scene backed by a TestClock so grace expiry is deterministic. The scene
        // clock (ms) is independent of the drain rate-window clock (`now_us`).
        let clock = TestClock::new(1_000);
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Live: drain materialises the governed surface under the driver lease.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);
        let lease = driver
            .lease_id
            .expect("driver holds a lease after the create drain");
        let tile = driver
            .drive
            .entries
            .get(projection_id)
            .expect("drive entry exists")
            .tile_scene_id
            .expect("create drain made a portal tile");
        assert_eq!(scene.tile_count(), 1, "portal surface is live");

        // Disconnect at the authority layer: the degraded window opens, but the
        // already-committed retained transcript is preserved (no committed loss).
        driver
            .authority_mut()
            .record_hud_connection(
                projection_id,
                HudConnectionMetadata {
                    connection_id: "connection-1".to_string(),
                    authenticated_session_id: "runtime-session-1".to_string(),
                    granted_capabilities: vec!["create_tiles".to_string()],
                    connected_at_wall_us: 20,
                    last_reconnect_wall_us: 20,
                },
            )
            .unwrap();
        driver
            .authority_mut()
            .mark_hud_disconnected(projection_id, 30)
            .unwrap();
        let degraded = driver
            .authority_mut()
            .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
            .expect("degraded portal state still materialises while within grace");
        assert!(
            degraded.connection_degraded,
            "the portal is degraded while disconnected (the window grace must bound)"
        );
        assert_eq!(
            degraded.visible_transcript.len(),
            1,
            "the committed unit is retained through the disconnect"
        );

        // Grace bound: orphan the lease, advance past grace, run the orphan reaper.
        scene.disconnect_lease(&lease, clock.now_millis()).unwrap();
        clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1_000);
        let expiries = scene.expire_leases();
        assert_eq!(expiries.len(), 1, "the grace-expired lease is reaped");
        assert!(
            expiries[0].removed_tiles.contains(&tile),
            "grace expiry removes the governed surface under the orphan path"
        );
        assert_eq!(scene.tile_count(), 0, "no surface survives past grace");
        assert!(
            !scene.lease_is_active(&lease),
            "the lease is no longer active after grace expiry"
        );

        // Authority-side surface removal that production owes on grace expiry:
        // expire the projection. After this there is NO further
        // `ProjectedPortalState` — stale content can never be re-materialised.
        assert!(
            driver.authority_mut().expire_projection(projection_id),
            "expire_projection removes the resident projection session"
        );
        assert!(
            driver
                .authority_mut()
                .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
                .is_none(),
            "a grace-expired portal produces no further ProjectedPortalState"
        );

        // A subsequent drain with no live session resurrects nothing.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 400);
        assert_eq!(
            scene.tile_count(),
            0,
            "draining after grace expiry must not revive the removed surface"
        );
    }

    /// hud-xlx1r (tasks 4.x + AC#2): headless disconnect → reconnect *within*
    /// grace at the driver+scene integration layer. The same governed surface
    /// (tile) resumes coherently — the already-committed transcript is preserved,
    /// a post-reconnect append lands on the SAME tile, and no committed unit is
    /// duplicated or reset.
    ///
    /// The authority-only reconnect tests (e.g.
    /// `reconnect_resumes_from_retained_window_and_clears_stale_treatment`) prove
    /// the transcript bookkeeping in isolation; this test proves the *rendered
    /// surface* survives the scene-lease orphan/reconnect and repaints both units
    /// on the original tile.
    #[test]
    fn disconnect_then_reconnect_within_grace_resumes_same_surface_without_duplication() {
        use std::sync::Arc;
        use tze_hud_projection::HudConnectionMetadata;
        use tze_hud_scene::{Clock, TestClock};

        const FIRST: &str = "ALPHA-committed-before-drop";
        const SECOND: &str = "BETA-continued-after-resume";

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let projection_id = "proj-resume";
        let token = attach_and_get_token(&mut driver, projection_id);
        driver.attach_projection(projection_id, Vec::new());

        let clock = TestClock::new(1_000);
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Connect + commit a unit + first drain → tile painted with FIRST.
        driver
            .authority_mut()
            .record_hud_connection(
                projection_id,
                HudConnectionMetadata {
                    connection_id: "connection-1".to_string(),
                    authenticated_session_id: "runtime-session-1".to_string(),
                    granted_capabilities: vec!["create_tiles".to_string()],
                    connected_at_wall_us: 20,
                    last_reconnect_wall_us: 20,
                },
            )
            .unwrap();
        publish(&mut driver, projection_id, &token, FIRST, 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);
        let tile = driver
            .drive
            .entries
            .get(projection_id)
            .expect("drive entry exists")
            .tile_scene_id
            .expect("create drain made a portal tile");
        let lease = driver.lease_id.expect("driver holds a lease");

        // Disconnect: scene orphans the surface (badge), authority opens grace.
        scene.disconnect_lease(&lease, clock.now_millis()).unwrap();
        driver
            .authority_mut()
            .mark_hud_disconnected(projection_id, 300)
            .unwrap();
        assert!(
            driver
                .authority_mut()
                .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
                .expect("degraded state materialises")
                .connection_degraded,
            "portal is degraded while disconnected"
        );

        // Reconnect WITHIN grace (well under DEFAULT_GRACE_PERIOD_MS).
        clock.advance(5_000);
        scene.reconnect_lease(&lease, clock.now_millis()).unwrap();
        driver
            .authority_mut()
            .record_hud_connection(
                projection_id,
                HudConnectionMetadata {
                    connection_id: "connection-resumed".to_string(),
                    authenticated_session_id: "runtime-session-resumed".to_string(),
                    granted_capabilities: vec!["create_tiles".to_string()],
                    connected_at_wall_us: 40,
                    last_reconnect_wall_us: 40,
                },
            )
            .unwrap();

        // Resume: append a new committed unit and drain again. The publish/drain
        // timestamps are spaced past the rate window so the second update is
        // serviced (mirrors `drain_paints_published_transcript_onto_tile`).
        publish(
            &mut driver,
            projection_id,
            &token,
            SECOND,
            PORTAL_UPDATE_RATE_WINDOW_WALL_US + 100,
        );
        driver.drain_inner(
            &mut scene,
            &mut processor,
            Some(tab_id),
            PORTAL_UPDATE_RATE_WINDOW_WALL_US * 2 + 1,
        );

        // The resumed surface is the SAME tile, not a fresh one (no reset).
        let resumed_tile = driver
            .drive
            .entries
            .get(projection_id)
            .expect("drive entry persists across reconnect")
            .tile_scene_id
            .expect("resumed portal still has its tile");
        assert_eq!(
            resumed_tile, tile,
            "resume reuses the original surface — reconnect within grace is not a reset"
        );
        assert_eq!(
            scene.tile_count(),
            1,
            "exactly one live portal tile after resume"
        );

        // Degraded treatment cleared, interaction re-enabled.
        let resumed_state = driver
            .authority_mut()
            .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
            .expect("resumed state materialises");
        assert!(
            !resumed_state.connection_degraded,
            "reconnect clears the degraded/stale treatment"
        );
        assert!(
            resumed_state.interaction_enabled,
            "live presentation re-enables input after resume"
        );

        // Both committed units are present, each exactly once — coherent resume,
        // no duplication or reset of the retained transcript.
        let content = tile_markdown(&scene, tile);
        assert_eq!(
            content.matches(FIRST).count(),
            1,
            "the pre-disconnect committed unit is retained exactly once (no duplication)"
        );
        assert_eq!(
            content.matches(SECOND).count(),
            1,
            "the post-reconnect append appears exactly once"
        );
    }

    /// hud-i429x (openspec portal-disconnect-resume-ux §3.2): the PRODUCTION
    /// path — `mark_all_projections_disconnected` (the whole-channel ungraceful
    /// drop trigger, hud-5i16d) followed by the per-frame `drain` sweep — bounds
    /// the degraded window by the lease grace and removes the surface on grace
    /// expiry, after which the projection yields NO further `ProjectedPortalState`.
    ///
    /// The sibling `disconnected_portal_surface_removed_on_grace_expiry_yields_no_further_state`
    /// invokes the scene/authority steps directly; THIS test proves the reaper is
    /// actually wired into `drain` — an ungraceful drop on an idle portal reaps
    /// itself with no manual `disconnect_lease`/`expire_leases` calls and no
    /// further owner traffic.
    #[test]
    fn production_ungraceful_drop_reaps_surface_on_grace_expiry_via_drain_sweep() {
        use std::sync::Arc;
        use tze_hud_projection::HudConnectionMetadata;
        use tze_hud_scene::TestClock;

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let projection_id = "proj-prod-grace";
        let token = attach_and_get_token(&mut driver, projection_id);
        driver.attach_projection(projection_id, Vec::new());
        driver
            .authority_mut()
            .record_hud_connection(
                projection_id,
                HudConnectionMetadata {
                    connection_id: "connection-1".to_string(),
                    authenticated_session_id: "runtime-session-1".to_string(),
                    granted_capabilities: vec!["create_tiles".to_string()],
                    connected_at_wall_us: 20,
                    last_reconnect_wall_us: 20,
                },
            )
            .unwrap();
        publish(
            &mut driver,
            projection_id,
            &token,
            "committed before drop",
            100,
        );

        let clock = TestClock::new(1_000);
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        // Live: first drain materialises the tile under a fresh driver lease.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 200);
        let lease = driver
            .lease_id
            .expect("driver holds a lease after create drain");
        assert_eq!(scene.tile_count(), 1, "portal surface is live");
        assert!(
            scene.lease_is_active(&lease),
            "driver lease is active while live"
        );

        // PRODUCTION TRIGGER: the MCP portal_op channel closed ungracefully.
        driver.mark_all_projections_disconnected();

        // Next drain: the sweep orphans the lease (grace opens) and the forced
        // repaint dims the transcript. The surface is retained — degraded, not gone.
        let version_before_orphan = scene.version;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 300);
        assert_eq!(
            scene.tile_count(),
            1,
            "surface retained during grace (degraded, not removed)"
        );
        assert!(
            scene.lease_is_orphaned(&lease),
            "sweep orphaned the driver lease — grace clock is now running"
        );
        assert!(
            scene.version > version_before_orphan,
            "orphan + degraded repaint bump the scene version so the dim paints"
        );
        assert!(
            driver
                .authority_mut()
                .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
                .expect("degraded state materialises within grace")
                .connection_degraded,
            "the portal is degraded while within the grace window"
        );

        // Advance past grace, then drain: the sweep reaps the surface.
        clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1_000);
        let version_before_reap = scene.version;
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 400);
        assert_eq!(
            scene.tile_count(),
            0,
            "grace expiry removes the governed surface"
        );
        assert!(
            scene.version > version_before_reap,
            "reaping bumps the scene version so the removal paints even on an idle frame"
        );
        assert!(
            !scene.lease_is_active(&lease),
            "the reaped lease is no longer active"
        );
        assert!(
            !driver.drive.entries.contains_key(projection_id),
            "the reaped projection's drive entry is dropped"
        );
        assert!(
            driver
                .authority_mut()
                .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
                .is_none(),
            "a grace-reaped portal produces no further ProjectedPortalState"
        );

        // A further drain revives nothing.
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 500);
        assert_eq!(
            scene.tile_count(),
            0,
            "draining after reap does not revive the surface"
        );
    }

    /// hud-ccj2o: agent-side silence uses the same governed orphan/grace window
    /// as an explicit upstream drop. The liveness threshold opens Degraded; it
    /// does not create a second independent surface-expiry timer.
    #[test]
    fn agent_liveness_degradation_is_bounded_by_existing_lease_grace() {
        use std::sync::Arc;
        use tze_hud_scene::TestClock;

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                agent_liveness_degraded_after_wall_us: 1_000_000,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let projection_id = "proj-agent-liveness-grace";
        let token = attach_and_get_token(&mut driver, projection_id);
        driver.attach_projection(projection_id, Vec::new());
        publish(
            &mut driver,
            projection_id,
            &token,
            "last live output",
            1_100,
        );

        let clock = TestClock::new(1_000);
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_100);
        assert_eq!(
            driver.next_wake_deadline(&scene),
            Some(PortalWakeDeadline {
                wall_us: 1_001_100,
                family: PortalDeadlineFamily::AgentLiveness,
            }),
            "an idle runtime must schedule the first liveness sweep"
        );
        let lease = driver
            .lease_id
            .expect("first drain grants the portal lease");
        assert_eq!(scene.tile_count(), 1);
        assert!(scene.lease_is_active(&lease));

        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_001_100);
        assert!(
            scene.lease_is_orphaned(&lease),
            "the authority liveness transition must open the existing scene lease grace"
        );
        assert!(
            driver
                .authority_mut()
                .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
                .expect("stale portal is retained during grace")
                .connection_degraded
        );
        assert_eq!(scene.tile_count(), 1, "stale content remains during grace");
        let scene_lease = scene
            .leases
            .get(&lease)
            .expect("lease remains during grace");
        let expected_grace_deadline_us = scene_lease
            .disconnected_at_ms
            .expect("degradation starts the grace clock")
            .saturating_add(scene_lease.grace_period_ms)
            .saturating_mul(1_000);
        assert_eq!(
            driver.next_wake_deadline(&scene),
            Some(PortalWakeDeadline {
                wall_us: expected_grace_deadline_us,
                family: PortalDeadlineFamily::LeaseLifecycle,
            }),
            "after degradation the idle runtime must wake at the same governed grace boundary"
        );

        clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_300);
        assert_eq!(
            scene.tile_count(),
            0,
            "lease grace expiry removes stale content"
        );
        assert!(
            driver
                .authority_mut()
                .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
                .is_none(),
            "the grace-reaped projection cannot rematerialize stale state"
        );
    }

    /// T5: `hud_hold` on a portal keeps it past the idle reap window
    /// (liveness threshold + lease grace) with its transcript intact; once the
    /// hold lapses the ordinary liveness/grace path reclaims it.
    #[test]
    fn portal_hold_outlives_idle_reap_window_then_reclaims() {
        use std::sync::Arc;
        use tze_hud_scene::TestClock;

        const T0_US: u64 = 1_000_000;
        const HOLD_MS: u64 = 300_000;
        let mut driver = InProcessPortalDriver::new();
        let projection_id = "proj-held";
        let token = attach_and_get_token_at(&mut driver, projection_id, T0_US);
        driver.attach_projection(projection_id, Vec::new());
        publish(&mut driver, projection_id, &token, "kept transcript", T0_US);

        let clock = TestClock::new(T0_US / 1_000);
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), T0_US);
        let lease = driver
            .lease_id
            .expect("first drain grants the portal lease");

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op_at(
            PortalOp::Hold {
                projection_id: projection_id.to_string(),
                owner_token: token.clone(),
                ttl_ms: HOLD_MS,
                reply: tx,
            },
            T0_US,
        );
        assert!(rx.try_recv().expect("hold replies").is_ok());
        let hold_end_us = T0_US + HOLD_MS * 1_000;
        assert_eq!(
            driver.next_wake_deadline(&scene),
            Some(PortalWakeDeadline {
                wall_us: hold_end_us,
                family: PortalDeadlineFamily::AgentLiveness,
            }),
            "a held portal's first liveness sweep is the hold deadline"
        );

        // Past the unheld reap window (30 s liveness + 30 s grace), within the hold.
        let idle_reap_ms = ProjectionBounds::default().agent_liveness_degraded_after_wall_us
            / 1_000
            + SceneGraph::DEFAULT_GRACE_PERIOD_MS;
        let mid_ms = 2 * idle_reap_ms;
        assert!(mid_ms < HOLD_MS);
        clock.advance(mid_ms);
        driver.drain_inner(
            &mut scene,
            &mut processor,
            Some(tab_id),
            T0_US + mid_ms * 1_000,
        );
        assert!(
            scene.lease_is_active(&lease),
            "held portal lease stays active"
        );
        assert!(!driver.authority.is_agent_liveness_degraded(projection_id));
        assert_eq!(scene.tile_count(), 1);
        let transcript = driver
            .authority
            .visible_transcript_window(projection_id)
            .expect("held portal keeps its session");
        assert!(
            transcript
                .iter()
                .any(|u| u.output_text == "kept transcript")
        );

        // The hold lapses: liveness degrades, the lease orphans, grace reaps.
        clock.advance(HOLD_MS - mid_ms);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), hold_end_us);
        assert!(driver.authority.is_agent_liveness_degraded(projection_id));
        assert!(scene.lease_is_orphaned(&lease));
        clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1);
        driver.drain_inner(
            &mut scene,
            &mut processor,
            Some(tab_id),
            hold_end_us + (SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1) * 1_000,
        );
        assert_eq!(scene.tile_count(), 0, "expired hold is reclaimed");
        assert!(
            driver
                .authority
                .visible_transcript_window(projection_id)
                .is_none()
        );
    }

    /// `ttl_ms: 0` holds a portal until it is cleared: no liveness deadline.
    #[test]
    fn portal_hold_without_ttl_never_degrades() {
        let mut driver = InProcessPortalDriver::new();
        let projection_id = "proj-held-forever";
        let token = attach_and_get_token_at(&mut driver, projection_id, 1_000);
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op_at(
            PortalOp::Hold {
                projection_id: projection_id.to_string(),
                owner_token: token,
                ttl_ms: 0,
                reply: tx,
            },
            1_000,
        );
        assert!(rx.try_recv().expect("hold replies").is_ok());
        assert_eq!(
            driver.authority.next_agent_liveness_deadline_wall_us(),
            None
        );
        assert!(
            driver
                .authority
                .sweep_agent_liveness_degradation(u64::MAX - 1)
                .is_empty()
        );

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        driver.dispatch_portal_op_at(
            PortalOp::Hold {
                projection_id: projection_id.to_string(),
                owner_token: "wrong".to_string(),
                ttl_ms: 0,
                reply: tx,
            },
            2_000,
        );
        assert_eq!(
            rx.try_recv().expect("hold replies").unwrap_err().error_code,
            ProjectionErrorCode::ProjectionUnauthorized
        );
    }

    #[test]
    fn terminal_resident_lease_reconciles_without_past_deadline_spin() {
        use std::sync::Arc;
        use tze_hud_scene::TestClock;

        let mut driver = InProcessPortalDriver::new();
        let projection_id = "resident-ttl-expiry";
        let token = attach_and_get_token(&mut driver, projection_id);
        driver.attach_projection(projection_id, Vec::new());
        publish(&mut driver, projection_id, &token, "resident", 1_000_000);

        let clock = TestClock::new(1_000);
        let mut scene = SceneGraph::new_with_clock(800.0, 600.0, Arc::new(clock.clone()));
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_000_000);
        let lease_id = driver.drive.entries[projection_id]
            .scene_lease_id
            .expect("portal lease must exist");
        let ttl_ms = scene.leases[&lease_id].ttl_ms;
        assert!(ttl_ms >= 24 * 60 * 60 * 1_000);

        clock.advance(ttl_ms + 1);
        let expiries = scene.expire_leases();
        assert!(expiries.iter().any(|expiry| expiry.lease_id == lease_id));
        assert_eq!(scene.tile_count(), 0);
        assert_eq!(
            driver.next_wake_deadline(&scene).unwrap().family,
            PortalDeadlineFamily::ImmediateWork,
            "a compositor-reaped terminal lease requests one reconciliation drain, not its past TTL"
        );

        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_000_001);
        assert!(!driver.drive.entries.contains_key(projection_id));
        assert!(
            driver
                .authority_mut()
                .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
                .is_none()
        );
        assert_eq!(
            driver.next_wake_deadline(&scene),
            None,
            "reconciled terminal lease must not schedule a clamped past deadline"
        );
    }

    /// Review regression for hud-ccj2o: recovering one projection must not
    /// reconnect a stale sibling's grace clock. Each projection owns its own
    /// governed surface lifecycle even though both are driven by one runtime.
    #[test]
    fn partial_agent_liveness_recovery_reaps_only_the_stale_projection() {
        use std::sync::Arc;
        use tze_hud_scene::TestClock;

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                agent_liveness_degraded_after_wall_us: 1_000_000,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let projection_a = "proj-liveness-recovered";
        let projection_b = "proj-liveness-stale";
        let token_a = attach_and_get_token(&mut driver, projection_a);
        let token_b = attach_and_get_token(&mut driver, projection_b);
        driver.attach_projection(projection_a, Vec::new());
        driver.attach_projection(projection_b, Vec::new());
        publish(&mut driver, projection_a, &token_a, "live A", 1_100);
        publish(&mut driver, projection_b, &token_b, "live B", 1_100);

        let clock = TestClock::new(1_000);
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_100);
        assert_eq!(scene.tile_count(), 2, "both projections start live");

        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_001_100);
        assert!(driver.authority.is_agent_liveness_degraded(projection_a));
        assert!(driver.authority.is_agent_liveness_degraded(projection_b));

        // Authenticated owner traffic recovers A only, within scene lease grace.
        publish(
            &mut driver,
            projection_a,
            &token_a,
            "A recovered",
            1_001_101,
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_001_101);
        assert!(!driver.authority.is_agent_liveness_degraded(projection_a));
        assert!(driver.authority.is_agent_liveness_degraded(projection_b));

        clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_001_102);

        assert!(
            driver.drive.entries.contains_key(projection_a),
            "the authenticated projection must survive"
        );
        assert!(
            driver
                .authority
                .projected_portal_state(projection_a, &ProjectedPortalPolicy::permit_all())
                .is_some(),
            "the authenticated projection must remain materialisable"
        );
        assert!(
            !driver.drive.entries.contains_key(projection_b),
            "the independently stale projection must be reaped after grace"
        );
        assert!(
            driver
                .authority
                .projected_portal_state(projection_b, &ProjectedPortalPolicy::permit_all())
                .is_none(),
            "the grace-reaped sibling must not rematerialise stale state"
        );
        assert_eq!(scene.tile_count(), 1, "only A's governed surface remains");
    }

    /// Review regression for hud-ccj2o: a bridge-routed projection has no local
    /// tile, but it still needs the same SceneGraph lease-grace governance. On
    /// expiry the remote materialiser receives one tombstone and both runtime
    /// state holders forget the projection.
    #[test]
    fn bridged_agent_liveness_grace_expiry_emits_detach_and_reaps_state() {
        use std::sync::Arc;
        use tze_hud_scene::TestClock;

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                agent_liveness_degraded_after_wall_us: 1_000_000,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeMessage>(16);
        driver.set_resident_grpc_bridge_tx(Some(tx));

        let projection_id = "proj-bridged-liveness-grace";
        let token = attach_and_get_token(&mut driver, projection_id);
        driver.attach_projection(projection_id, Vec::new());
        driver.set_projection_transport(projection_id, PortalTransport::ResidentGrpcBridge);
        publish(&mut driver, projection_id, &token, "bridged live", 1_100);

        let clock = TestClock::new(1_000);
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_100);
        assert!(matches!(rx.try_recv(), Ok(BridgeMessage::Publish { .. })));
        assert_eq!(scene.tile_count(), 0, "bridge path owns the visible tile");

        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_001_100);
        let degraded = rx.try_recv().expect("degraded state is forwarded");
        assert!(matches!(
            degraded,
            BridgeMessage::Publish { ref state, .. } if state.connection_degraded
        ));

        clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_001_101);

        let mut detaches = 0;
        while let Ok(message) = rx.try_recv() {
            if let BridgeMessage::Detach { projection_id: id } = message {
                assert_eq!(id, projection_id);
                detaches += 1;
            }
        }
        assert_eq!(
            detaches, 1,
            "grace expiry emits exactly one remote tombstone"
        );
        assert!(!driver.drive.entries.contains_key(projection_id));
        assert!(
            driver
                .authority
                .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
                .is_none(),
            "grace expiry removes authority state too"
        );

        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_001_102);
        assert!(rx.try_recv().is_err(), "reaping is one-shot");
    }

    /// The bridge shadow lease follows the same resume contract as a local
    /// surface: authenticated traffic within grace reconnects that projection's
    /// lease, keeps authority/drive state, and suppresses the expiry tombstone.
    #[test]
    fn bridged_agent_liveness_recovery_within_grace_prevents_detach() {
        use std::sync::Arc;
        use tze_hud_scene::TestClock;

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                agent_liveness_degraded_after_wall_us: 1_000_000,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BridgeMessage>(16);
        driver.set_resident_grpc_bridge_tx(Some(tx));

        let projection_id = "proj-bridged-liveness-resume";
        let token = attach_and_get_token(&mut driver, projection_id);
        driver.attach_projection(projection_id, Vec::new());
        driver.set_projection_transport(projection_id, PortalTransport::ResidentGrpcBridge);
        publish(&mut driver, projection_id, &token, "bridged live", 1_100);

        let clock = TestClock::new(1_000);
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_100);
        while rx.try_recv().is_ok() {}

        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_001_100);
        while rx.try_recv().is_ok() {}

        clock.advance(5_000);
        publish(
            &mut driver,
            projection_id,
            &token,
            "bridged recovered",
            1_001_101,
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_001_101);
        assert!(!driver.authority.is_agent_liveness_degraded(projection_id));
        while rx.try_recv().is_ok() {}

        clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1);
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), 1_001_102);

        assert!(
            driver.drive.entries.contains_key(projection_id),
            "recovery within grace keeps drive state"
        );
        assert!(
            driver
                .authority
                .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
                .is_some(),
            "recovery within grace keeps authority state"
        );
        let mut detaches = 0;
        while let Ok(message) = rx.try_recv() {
            if matches!(message, BridgeMessage::Detach { .. }) {
                detaches += 1;
            }
        }
        assert_eq!(
            detaches, 0,
            "a reconnected bridge projection must not receive a grace-expiry tombstone"
        );
    }

    /// hud-i429x + hud-xlx1r: the production sweep must NOT break resume — an
    /// ungraceful drop followed by owner re-attach WITHIN grace resumes the SAME
    /// surface (the sweep reconnects the orphaned lease) rather than reaping or
    /// starting a fresh portal.
    #[test]
    fn production_reattach_within_grace_resumes_same_surface_via_drain_sweep() {
        use std::sync::Arc;
        use tze_hud_projection::HudConnectionMetadata;
        use tze_hud_scene::TestClock;

        const FIRST: &str = "ALPHA-before-drop";

        let mut driver = InProcessPortalDriver {
            authority: ProjectionAuthority::new(ProjectionBounds {
                max_portal_updates_per_second: 100,
                ..ProjectionBounds::default()
            })
            .unwrap(),
            drive: InProcessPortalDriveState::new(),
            lease_id: None,
            portal_publish_to_present_latency: LatencyBucket::new("portal_publish_to_present"),
            drain_deferral_count: 0,
            resident_grpc_bridge_tx: None,
        };

        let projection_id = "proj-prod-resume";

        // Attach + first publish via the REAL op path so the owner token lives in
        // the same wall-clock domain the op path validates against — a later owner
        // publish then authenticates instead of tripping token-expiry. (The grace
        // clock is a SEPARATE scene `TestClock`, advanced deterministically below.)
        let (attach_tx, mut attach_rx) =
            tokio::sync::oneshot::channel::<Result<String, PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::Attach {
            projection_id: projection_id.to_string(),
            display_name: "Test proj-prod-resume".to_string(),
            idempotency_key: None,
            provider_kind: None,
            content_classification: None,
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            reply: attach_tx,
        });
        let token = attach_rx
            .try_recv()
            .expect("attach reply delivered synchronously")
            .expect("attach accepted");
        // Record a live HUD connection so a later ungraceful drop derives
        // `connection_degraded` (mirrors an owner that had a live connection).
        driver
            .authority_mut()
            .record_hud_connection(
                projection_id,
                HudConnectionMetadata {
                    connection_id: "connection-1".to_string(),
                    authenticated_session_id: "runtime-session-1".to_string(),
                    granted_capabilities: vec!["create_tiles".to_string()],
                    connected_at_wall_us: now_wall_us(),
                    last_reconnect_wall_us: now_wall_us(),
                },
            )
            .unwrap();
        let (pub_tx, mut pub_rx) = tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: projection_id.to_string(),
            owner_token: token.clone(),
            output_text: FIRST.to_string(),
            logical_unit_id: Some("unit-1".to_string()),
            output_kind: None,
            content_classification: None,
            coalesce_key: None,
            expects_reply: None,
            reply: pub_tx,
        });
        pub_rx
            .try_recv()
            .expect("publish reply delivered synchronously")
            .expect("first owner publish accepted");

        let clock = TestClock::new(1_000);
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let mut processor = InputProcessor::new();

        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), now_wall_us());
        let lease = driver.lease_id.expect("driver holds a lease");
        let tile = driver
            .drive
            .entries
            .get(projection_id)
            .expect("drive entry exists")
            .tile_scene_id
            .expect("create drain made a portal tile");

        // Ungraceful drop → next drain orphans the lease.
        driver.mark_all_projections_disconnected();
        driver.drain_inner(&mut scene, &mut processor, Some(tab_id), now_wall_us());
        assert!(
            scene.lease_is_orphaned(&lease),
            "lease orphaned by the drop sweep"
        );

        // Owner publishes again WITHIN grace. A successful owner PublishOutput op
        // is the production reconnect signal: `dispatch_portal_op` →
        // `clear_projection_disconnect_at` clears `hud_disconnected` and records
        // the connection SYNCHRONOUSLY at dispatch (independent of the later
        // content due-cycle).
        clock.advance(5_000);
        let (pub_tx2, mut pub_rx2) =
            tokio::sync::oneshot::channel::<Result<(), PortalOpRejection>>();
        driver.dispatch_portal_op(PortalOp::PublishOutput {
            projection_id: projection_id.to_string(),
            owner_token: token.clone(),
            output_text: "BETA-after-resume".to_string(),
            logical_unit_id: Some("unit-resume".to_string()),
            output_kind: None,
            content_classification: None,
            coalesce_key: None,
            expects_reply: None,
            reply: pub_tx2,
        });
        pub_rx2
            .try_recv()
            .expect("publish reply delivered synchronously")
            .expect("owner publish within grace must be accepted");
        // Drain far enough past the rate window that the resumed update is DUE and
        // actually renders this cycle — so the assertion below exercises the real
        // render path (the reconnect must have restored an Active lease FIRST, or
        // `set_tile_root_checked` would reject the batch and drop the content).
        driver.drain_inner(
            &mut scene,
            &mut processor,
            Some(tab_id),
            now_wall_us() + PORTAL_UPDATE_RATE_WINDOW_WALL_US * 2,
        );

        // Same surface, lease active again, degraded cleared — no reap, no reset.
        let resumed_tile = driver
            .drive
            .entries
            .get(projection_id)
            .expect("drive entry persists across reconnect")
            .tile_scene_id
            .expect("resumed portal still has its tile");
        assert_eq!(resumed_tile, tile, "resume reuses the original surface");
        assert_eq!(
            scene.tile_count(),
            1,
            "exactly one live portal tile after resume"
        );
        assert!(
            scene.lease_is_active(&lease),
            "the sweep reconnected the orphaned lease within grace"
        );
        assert!(
            !driver
                .authority_mut()
                .projected_portal_state(projection_id, &ProjectedPortalPolicy::permit_all())
                .expect("resumed state materialises")
                .connection_degraded,
            "reconnect within grace clears the degraded treatment"
        );

        // The resumed publish PAINTED onto the tile — proving the lease was
        // reconnected to Active BEFORE the due-loop rendered it. Reconnecting only
        // after rendering (end-of-drain) would make `set_tile_root_checked` reject
        // this batch under the still-Orphaned lease and the content would be lost.
        let content = tile_markdown(&scene, resumed_tile);
        assert!(
            content.contains("BETA-after-resume"),
            "the resumed publish paints under the reconnected Active lease (P1): got {content:?}"
        );
    }
}
