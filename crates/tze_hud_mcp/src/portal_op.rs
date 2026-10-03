//! Portal projection operation channel messages.
//!
//! This module defines the [`PortalOp`] enum that flows from the MCP HTTP
//! server task (async Tokio thread) to the winit event-loop thread via an
//! unbounded mpsc channel.
//!
//! ## Why is this in `tze_hud_mcp`?
//!
//! The MCP server needs to _send_ `PortalOp` values; the runtime
//! (`tze_hud_runtime`) needs to _receive_ and _dispatch_ them. Since
//! `tze_hud_runtime` depends on `tze_hud_mcp` (not the other way around),
//! placing the type here avoids a circular dependency while keeping the
//! channel definition close to the MCP tool implementations that produce it.
//!
//! The runtime re-exports `PortalOp` from
//! `tze_hud_runtime::portal_projection_driver`.
//!
//! ## Field types
//!
//! Request fields use plain [`String`] / [`Option<String>`]; the runtime
//! converts them to the appropriate projection types (`ProviderKind`,
//! `OutputKind`, etc.) before calling the authority.
//!
//! Reply channels are the one exception: they carry a typed
//! [`PortalOpRejection`] (wrapping a [`ProjectionErrorCode`]) on the error path
//! rather than a flattened `String`, so the tool layer can map it to the
//! shared error set (`error::map_projection`) and tell a stale owner token from
//! other failures. The portal's own codes never reach the model. This module
//! therefore depends on
//! `tze_hud_projection` for that single type; there is no dependency cycle
//! because the projection crate does not depend on `tze_hud_mcp`.

use tze_hud_projection::ProjectionErrorCode;

/// Structured rejection carried on a [`PortalOp`] reply channel's error path.
///
/// The tool layer maps it to a shared code and hint at the MCP boundary
/// (`error::map_projection`); the internal code is never shown to the model.
#[derive(Debug, Clone)]
pub struct PortalOpRejection {
    /// Stable projection error code. Either the authority's own
    /// `error_code`, or `ProjectionInvalidArgument` for a driver-side
    /// pre-authority validation failure (unrecognized enum string, etc.).
    pub error_code: ProjectionErrorCode,
    /// Human-readable detail (the authority `status_summary` or a driver-side
    /// validation message).
    pub message: String,
}

impl PortalOpRejection {
    /// Construct a rejection from a stable code and a human-readable message.
    pub fn new(error_code: ProjectionErrorCode, message: impl Into<String>) -> Self {
        Self {
            error_code,
            message: message.into(),
        }
    }
}

/// An operation dispatched to the in-process projection authority through the
/// `portal_op_tx` / `portal_op_rx` channel pair (hud-bq0gl.2).
///
/// The MCP layer sends one of these for each `portal_projection_*` tool call.
/// The winit event-loop thread drains
/// the channel on every `about_to_wait` iteration (via `drain_portal_ops`) and
/// applies the operation to the in-process `InProcessPortalDriver` before
/// running the normal drain loop.
///
/// ## Why a channel?
///
/// The `InProcessPortalDriver` lives on the winit event-loop thread and is
/// accessed synchronously from `about_to_wait`. The MCP HTTP server runs on a
/// Tokio async runtime thread. An unbounded mpsc channel is the correct bridge:
/// the MCP task sends and returns immediately; the event-loop thread drains
/// lock-free on each iteration without blocking.
///
/// ## Security
///
/// `owner_token` for `PublishOutput` is validated by the projection authority
/// (`handle_publish_output`). The MCP layer forwards it verbatim; the authority
/// rejects invalid or expired tokens before storing any content.
#[derive(Debug)]
pub enum PortalOp {
    /// List bounded summaries for projections owned by the resident caller.
    ///
    /// This is read-only recovery/reconciliation: it never changes a
    /// projection's owner, lease, or lifecycle state.
    List {
        /// One-shot response channel carrying only content-free summaries.
        reply: tokio::sync::oneshot::Sender<Result<ProjectionListBatch, PortalOpRejection>>,
    },
    /// Attach a new projection session to the in-process authority.
    Attach {
        /// Caller-assigned identifier (max 128 bytes, must be unique).
        projection_id: String,
        /// Human-readable name for the session.
        display_name: String,
        /// Optional idempotency key for replay-safe re-attach. A matching replay
        /// rotates the owner token without extending its original expiry.
        idempotency_key: Option<String>,
        /// LLM provider kind as a snake_case string (`codex`, `claude`,
        /// `opencode`, `other`). `None` defaults to `other`. An unrecognized
        /// value is rejected by the driver before the authority is called.
        provider_kind: Option<String>,
        /// Viewer-facing content classification as a snake_case string
        /// (`public`, `household`, `private`, `sensitive`). `None` defaults
        /// to `private` (safe-by-default). An unrecognized value is rejected.
        content_classification: Option<String>,
        /// Optional human-readable workspace hint (e.g. project directory).
        workspace_hint: Option<String>,
        /// Optional human-readable repository hint (e.g. repo URL or name).
        repository_hint: Option<String>,
        /// Optional icon profile hint for visual identity selection.
        icon_profile_hint: Option<String>,
        /// Optional HUD target hint for multi-display routing.
        hud_target: Option<String>,
        /// One-shot response channel: authority returns the `owner_token` on
        /// success (needed for subsequent `PublishOutput` calls), or a
        /// [`PortalOpRejection`] carrying the stable error code on failure.
        reply: tokio::sync::oneshot::Sender<Result<String, PortalOpRejection>>,
    },
    /// Publish output text to an existing projection session.
    PublishOutput {
        /// Projection identifier matching a prior successful `Attach`.
        projection_id: String,
        /// Owner token returned by the `Attach` response.
        owner_token: String,
        /// Text to append to the transcript.
        output_text: String,
        /// Optional logical-unit ID for idempotent replay detection.
        logical_unit_id: Option<String>,
        /// Optional output kind as a snake_case string (`assistant`, `tool`,
        /// `status`, `error`, `other`). The runtime parses this into
        /// `OutputKind`, defaulting to `assistant` when `None`. An
        /// unrecognized value is rejected by the runtime.
        output_kind: Option<String>,
        /// Optional viewer-facing content classification as a snake_case
        /// string (`public`, `household`, `private`, `sensitive`). The
        /// runtime parses this into `ContentClassification`, defaulting to
        /// the safe-by-default `private` when `None`. An unrecognized value
        /// is rejected by the runtime.
        content_classification: Option<String>,
        /// Optional coalesce key. When set, repeated publishes sharing the
        /// key collapse in-place into a single transcript unit rather than
        /// appending. `None` means append (no coalescing).
        coalesce_key: Option<String>,
        /// Optional question signal: `true` means this output is a question
        /// awaiting a viewer reply — a core presence semantic, not a chatbot
        /// affordance. `None`/`false` is the exact pre-existing behavior (no
        /// cue rendered); this is a backward-compatible opt-in (hud-jip0k).
        expects_reply: Option<bool>,
        /// One-shot response channel: `Ok(())` on success or a
        /// [`PortalOpRejection`] carrying the stable error code on
        /// validation / auth failure.
        reply: tokio::sync::oneshot::Sender<Result<(), PortalOpRejection>>,
    },
    /// Publish a lifecycle status to an existing projection session.
    ///
    /// This is step 3 of the cooperative workflow and the only way the owning
    /// LLM signals its lifecycle state (e.g. `active`, `degraded`/blocked,
    /// `attached`/waiting-for-input) to the viewer, driving earned-urgency and
    /// ambient-attention affordances on the portal. It stays on the existing
    /// MCP transport and is ambient, not interruptive.
    PublishStatus {
        /// Projection identifier matching a prior successful `Attach`.
        projection_id: String,
        /// Owner token returned by the `Attach` response.
        owner_token: String,
        /// Lifecycle state as a snake_case string (`attached`, `active`,
        /// `degraded`, `hud_unavailable`, `detached`, `cleanup_pending`,
        /// `expired`). The runtime parses this into `ProjectionLifecycleState`;
        /// an unrecognized value is rejected before the authority sees it.
        lifecycle_state: String,
        /// Optional human-readable status detail recorded with the lifecycle
        /// state. The authority rejects text exceeding its configured
        /// `max_status_text_bytes`.
        status_text: Option<String>,
        /// One-shot response channel. On success the authority returns the
        /// applied lifecycle state as a snake_case string (the round-trip echo),
        /// or a [`PortalOpRejection`] carrying the stable error code on
        /// validation / auth failure.
        reply: tokio::sync::oneshot::Sender<Result<String, PortalOpRejection>>,
    },
    /// Drain HUD-originated pending input for an existing projection session.
    ///
    /// This is the LLM-facing poll: the owning session asks the authority for
    /// any operator input that arrived from the projected portal. Items are
    /// transitioned to `Delivered` by the authority and must subsequently be
    /// acknowledged with [`PortalOp::AcknowledgeInput`].
    GetPendingInput {
        /// Projection identifier matching a prior successful `Attach`.
        projection_id: String,
        /// Owner token returned by the `Attach` response.
        owner_token: String,
        /// Optional cap on the number of items returned. The authority clamps
        /// this to its configured `max_poll_items`. `None` uses the authority
        /// default.
        max_items: Option<usize>,
        /// Optional cap on the total response byte budget. The authority clamps
        /// this to its configured `max_poll_response_bytes`. `None` uses the
        /// authority default.
        max_bytes: Option<usize>,
        /// One-shot response channel. On success the authority returns a
        /// [`PendingInputBatch`] (the delivered items plus remaining-count /
        /// remaining-bytes back-pressure hints). On failure, a
        /// [`PortalOpRejection`] carrying the stable error code (invalid /
        /// expired token, validation error, etc.).
        reply: tokio::sync::oneshot::Sender<Result<PendingInputBatch, PortalOpRejection>>,
    },
    /// Acknowledge a previously delivered input item for a projection session.
    ///
    /// The owning session reports the terminal disposition of an input item
    /// (`handled`, `rejected`) or defers it (`deferred`, optionally with a
    /// `not_before_wall_us` re-delivery floor). Terminal acknowledgement is
    /// idempotent for replay safety; a conflicting terminal ack is rejected.
    AcknowledgeInput {
        /// Projection identifier matching a prior successful `Attach`.
        projection_id: String,
        /// Owner token returned by the `Attach` response.
        owner_token: String,
        /// Identifier of the input item being acknowledged (from a prior
        /// `GetPendingInput` response).
        input_id: String,
        /// Acknowledgement state as a snake_case string (`handled`,
        /// `deferred`, `rejected`). The runtime parses this into
        /// `InputAckState`; an unrecognized value is rejected.
        ack_state: String,
        /// Optional human-readable message recorded with the acknowledgement.
        ack_message: Option<String>,
        /// Optional re-delivery floor (wall-clock µs). Valid only when
        /// `ack_state` is `deferred`; the authority rejects it otherwise.
        not_before_wall_us: Option<u64>,
        /// One-shot response channel: `Ok(())` on success or a
        /// [`PortalOpRejection`] carrying the stable error code on
        /// validation / auth / conflict failure.
        reply: tokio::sync::oneshot::Sender<Result<(), PortalOpRejection>>,
    },
    /// Hold a projection session (`hud_hold`): refresh its agent liveness and
    /// keep it from being degraded and reaped for `ttl_ms` (0 = until detach
    /// or cleanup). The driver computes the deadline on its own clock.
    Hold {
        /// Projection identifier matching a prior successful `Attach`.
        projection_id: String,
        /// Owner token returned by the `Attach` response.
        owner_token: String,
        /// Hold duration in milliseconds; 0 holds until detach or cleanup.
        ttl_ms: u64,
        /// One-shot response channel: `Ok(())` on success or a
        /// [`PortalOpRejection`] carrying the stable error code.
        reply: tokio::sync::oneshot::Sender<Result<(), PortalOpRejection>>,
    },
    /// Detach a projection session, purging its private state.
    ///
    /// Tears down the projection: the authority removes the session and its
    /// coalescer entry, and the driver drops the drive entry / tile mapping.
    /// After detach the `projection_id` is free to be re-attached.
    Detach {
        /// Projection identifier matching a prior successful `Attach`.
        projection_id: String,
        /// Owner token returned by the `Attach` response.
        owner_token: String,
        /// Human-readable detach reason.
        reason: String,
        /// One-shot response channel: `Ok(())` on success or a
        /// [`PortalOpRejection`] carrying the stable error code on
        /// validation / auth failure.
        reply: tokio::sync::oneshot::Sender<Result<(), PortalOpRejection>>,
    },
}

/// Content-free metadata for one projection returned by [`PortalOp::List`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectionListEntry {
    /// Stable projection identifier.
    pub projection_id: String,
    /// Human-readable projection name.
    pub display_name: String,
    /// Lifecycle state in snake_case wire form.
    pub lifecycle_state: String,
    /// Unread output unit count.
    pub unread_output_count: usize,
    /// Nonterminal pending input count.
    pub pending_input_count: usize,
}

/// Bounded caller-scoped result of [`PortalOp::List`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectionListBatch {
    /// Content-free summaries belonging to the calling agent.
    pub projections: Vec<ProjectionListEntry>,
}

/// A single HUD-originated input item returned by [`PortalOp::GetPendingInput`].
///
/// This is the transport-layer mirror of the projection authority's
/// `PendingInputItem`. The runtime driver maps the authority type into this
/// dependency-free shape (this module must not depend on `tze_hud_projection`,
/// see the module doc). The MCP tool serializes it verbatim into the JSON-RPC
/// response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingInputEntry {
    /// Stable identifier of this input item, used to acknowledge it later.
    pub input_id: String,
    /// Projection identifier this input was submitted to.
    pub projection_id: String,
    /// Operator-submitted text.
    pub submission_text: String,
    /// Wall-clock µs when the input was submitted from the HUD.
    pub submitted_at_wall_us: u64,
    /// Wall-clock µs when the input expires if not acknowledged.
    pub expires_at_wall_us: u64,
    /// Delivery state as a snake_case string (`delivered`, `deferred`, ...).
    pub delivery_state: String,
    /// Viewer-facing content classification as a snake_case string.
    pub content_classification: String,
}

/// Result of a [`PortalOp::GetPendingInput`] drain.
///
/// Carries the delivered items plus back-pressure hints describing input that
/// could not fit in this response's item / byte budget.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingInputBatch {
    /// Items transitioned to `Delivered` and returned in this poll.
    pub items: Vec<PendingInputEntry>,
    /// Number of still-pending items that did not fit this response budget.
    pub remaining_count: usize,
    /// Total byte size of still-pending items that did not fit.
    pub remaining_bytes: usize,
}
