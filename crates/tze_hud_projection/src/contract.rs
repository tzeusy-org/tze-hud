//! Wire contract types, error types, and pure validation helpers for the
//! cooperative HUD projection operation contract.
//!
//! Moved from `lib.rs` in the P-1 mechanical split (hud-d570a). No logic
//! changes — byte-identical relocation with the minimal visibility adjustments
//! required by Rust's module privacy rules (`pub(super)` where a method or
//! helper is called from sibling modules that remain in `lib.rs` for this step).

use serde::{Deserialize, Serialize};
use std::fmt;
use subtle::ConstantTimeEq;
use thiserror::Error;

/// Stable append-only projection error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProjectionErrorCode {
    ProjectionNotFound,
    ProjectionAlreadyAttached,
    ProjectionUnauthorized,
    ProjectionTokenExpired,
    ProjectionInvalidArgument,
    ProjectionOutputTooLarge,
    ProjectionInputTooLarge,
    ProjectionInputQueueFull,
    ProjectionRateLimited,
    ProjectionStateConflict,
    ProjectionHudUnavailable,
    ProjectionInternalError,
}

impl ProjectionErrorCode {
    /// Stable wire string for this error code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProjectionNotFound => "PROJECTION_NOT_FOUND",
            Self::ProjectionAlreadyAttached => "PROJECTION_ALREADY_ATTACHED",
            Self::ProjectionUnauthorized => "PROJECTION_UNAUTHORIZED",
            Self::ProjectionTokenExpired => "PROJECTION_TOKEN_EXPIRED",
            Self::ProjectionInvalidArgument => "PROJECTION_INVALID_ARGUMENT",
            Self::ProjectionOutputTooLarge => "PROJECTION_OUTPUT_TOO_LARGE",
            Self::ProjectionInputTooLarge => "PROJECTION_INPUT_TOO_LARGE",
            Self::ProjectionInputQueueFull => "PROJECTION_INPUT_QUEUE_FULL",
            Self::ProjectionRateLimited => "PROJECTION_RATE_LIMITED",
            Self::ProjectionStateConflict => "PROJECTION_STATE_CONFLICT",
            Self::ProjectionHudUnavailable => "PROJECTION_HUD_UNAVAILABLE",
            Self::ProjectionInternalError => "PROJECTION_INTERNAL_ERROR",
        }
    }
}

impl fmt::Display for ProjectionErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Initial stable error-code set required by the projection contract.
pub const INITIAL_ERROR_CODES: [ProjectionErrorCode; 12] = [
    ProjectionErrorCode::ProjectionNotFound,
    ProjectionErrorCode::ProjectionAlreadyAttached,
    ProjectionErrorCode::ProjectionUnauthorized,
    ProjectionErrorCode::ProjectionTokenExpired,
    ProjectionErrorCode::ProjectionInvalidArgument,
    ProjectionErrorCode::ProjectionOutputTooLarge,
    ProjectionErrorCode::ProjectionInputTooLarge,
    ProjectionErrorCode::ProjectionInputQueueFull,
    ProjectionErrorCode::ProjectionRateLimited,
    ProjectionErrorCode::ProjectionStateConflict,
    ProjectionErrorCode::ProjectionHudUnavailable,
    ProjectionErrorCode::ProjectionInternalError,
];

/// Provider-neutral projection operation names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionOperation {
    Attach,
    List,
    PublishOutput,
    PublishStatus,
    GetPendingInput,
    AcknowledgeInput,
    Detach,
}

/// LLM provider kind. Provider-specific behavior must stay outside the core
/// projection operation semantics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Codex,
    Claude,
    Opencode,
    Other,
}

/// Projection lifecycle visible through bounded status summaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionLifecycleState {
    Attached,
    Active,
    Degraded,
    HudUnavailable,
    Detached,
    CleanupPending,
    Expired,
}

/// Viewer-facing classification. Missing classification defaults to private.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentClassification {
    Public,
    Household,
    #[default]
    Private,
    Sensitive,
}

/// Output kind for published transcript units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    #[default]
    Assistant,
    Tool,
    Status,
    Error,
    Other,
    /// Text submitted by the on-screen viewer via the HUD composer. Echoed into
    /// the transcript by `submit_portal_input` so the conversation is not
    /// structurally one-sided on the portal surface.
    Viewer,
}

/// Acknowledgement state accepted from the owning LLM session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAckState {
    Handled,
    Deferred,
    Rejected,
}

/// Daemon-owned input delivery state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDeliveryState {
    Pending,
    Delivered,
    Deferred,
    Handled,
    Rejected,
    Expired,
}

impl InputDeliveryState {
    pub(super) fn is_terminal(self) -> bool {
        matches!(self, Self::Handled | Self::Rejected | Self::Expired)
    }
}

/// Cooperative projected-session adapter family. The v1 projection path is a
/// text-stream portal adapter, not a PTY or terminal-capture adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectedPortalAdapterFamily {
    CooperativeProjection,
}

/// Runtime authority used to publish projected portal mutations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectedPortalRuntimeAuthority {
    ResidentSessionLease,
}

/// Projected portals render as content-layer territory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectedPortalLayer {
    Content,
}

/// Expanded or collapsed projected-portal presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectedPortalPresentation {
    Expanded,
    Collapsed,
}

/// Ambient attention state exposed by projected portals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectedPortalAttention {
    Ambient,
}

/// Local-first HUD composer feedback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortalInputFeedbackState {
    Accepted,
    Rejected,
}

/// Configurable contract bounds. Defaults match the v1 OpenSpec values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionBounds {
    pub max_output_bytes_per_call: usize,
    pub max_status_text_bytes: usize,
    pub max_retained_transcript_bytes: usize,
    pub max_visible_transcript_bytes: usize,
    pub max_pending_input_items: usize,
    pub max_pending_input_bytes_per_item: usize,
    pub max_pending_input_total_bytes: usize,
    pub max_poll_items: usize,
    pub max_poll_response_bytes: usize,
    pub max_list_items: usize,
    pub max_portal_updates_per_second: u32,
    pub max_seen_logical_units: usize,
    pub owner_token_ttl_wall_us: u64,
    /// Maximum wall-clock gap between authenticated owner operations before the
    /// projection is presented as stale. Applied when the authority is built.
    pub agent_liveness_degraded_after_wall_us: u64,
}

impl Default for ProjectionBounds {
    fn default() -> Self {
        Self {
            max_output_bytes_per_call: crate::DEFAULT_MAX_OUTPUT_BYTES_PER_CALL,
            max_status_text_bytes: crate::DEFAULT_MAX_STATUS_TEXT_BYTES,
            max_retained_transcript_bytes: crate::DEFAULT_MAX_RETAINED_TRANSCRIPT_BYTES,
            max_visible_transcript_bytes: crate::DEFAULT_MAX_VISIBLE_TRANSCRIPT_BYTES,
            max_pending_input_items: crate::DEFAULT_MAX_PENDING_INPUT_ITEMS,
            max_pending_input_bytes_per_item: crate::DEFAULT_MAX_PENDING_INPUT_BYTES_PER_ITEM,
            max_pending_input_total_bytes: crate::DEFAULT_MAX_PENDING_INPUT_TOTAL_BYTES,
            max_poll_items: crate::DEFAULT_MAX_POLL_ITEMS,
            max_poll_response_bytes: crate::DEFAULT_MAX_POLL_RESPONSE_BYTES,
            max_list_items: crate::DEFAULT_MAX_LIST_ITEMS,
            max_portal_updates_per_second: crate::DEFAULT_MAX_PORTAL_UPDATES_PER_SECOND,
            max_seen_logical_units: crate::DEFAULT_MAX_SEEN_LOGICAL_UNITS,
            owner_token_ttl_wall_us: crate::DEFAULT_OWNER_TOKEN_TTL_WALL_US,
            agent_liveness_degraded_after_wall_us:
                crate::DEFAULT_AGENT_LIVENESS_DEGRADED_AFTER_WALL_US,
        }
    }
}

impl ProjectionBounds {
    pub fn validate(&self) -> Result<(), ProjectionContractError> {
        if self.max_output_bytes_per_call == 0
            || self.max_status_text_bytes == 0
            || self.max_retained_transcript_bytes == 0
            || self.max_visible_transcript_bytes == 0
            || self.max_pending_input_items == 0
            || self.max_pending_input_bytes_per_item == 0
            || self.max_pending_input_total_bytes == 0
            || self.max_poll_items == 0
            || self.max_poll_response_bytes == 0
            || self.max_list_items == 0
            || self.max_portal_updates_per_second == 0
            || self.max_seen_logical_units == 0
            || self.owner_token_ttl_wall_us == 0
        {
            return Err(ProjectionContractError::InvalidArgument(
                "projection bounds must be non-zero".to_string(),
            ));
        }
        if self.max_visible_transcript_bytes > self.max_retained_transcript_bytes {
            return Err(ProjectionContractError::InvalidArgument(
                "visible transcript bound cannot exceed retained transcript bound".to_string(),
            ));
        }
        // Strict FIFO delivery cannot make progress if an accepted item cannot
        // fit in a single capped poll response.
        if self.max_poll_response_bytes < self.max_pending_input_bytes_per_item {
            return Err(ProjectionContractError::InvalidArgument(
                "max_poll_response_bytes cannot be smaller than \
                 max_pending_input_bytes_per_item"
                    .to_string(),
            ));
        }
        if !(crate::MIN_AGENT_LIVENESS_DEGRADED_AFTER_WALL_US
            ..=crate::MAX_AGENT_LIVENESS_DEGRADED_AFTER_WALL_US)
            .contains(&self.agent_liveness_degraded_after_wall_us)
        {
            return Err(ProjectionContractError::InvalidArgument(format!(
                "agent liveness threshold must be within {}..={} wall-clock microseconds",
                crate::MIN_AGENT_LIVENESS_DEGRADED_AFTER_WALL_US,
                crate::MAX_AGENT_LIVENESS_DEGRADED_AFTER_WALL_US
            )));
        }
        Ok(())
    }
}

/// Shared request envelope fields present on every operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationEnvelope {
    pub operation: ProjectionOperation,
    pub projection_id: String,
    pub request_id: String,
    pub client_timestamp_wall_us: u64,
}

impl OperationEnvelope {
    fn validate(&self, expected: ProjectionOperation) -> Result<(), ProjectionContractError> {
        if self.operation != expected {
            return Err(ProjectionContractError::InvalidArgument(format!(
                "operation must be {expected:?}"
            )));
        }
        validate_non_empty_bounded(
            "projection_id",
            &self.projection_id,
            crate::MAX_PROJECTION_ID_BYTES,
        )?;
        validate_non_empty_bounded("request_id", &self.request_id, crate::MAX_REQUEST_ID_BYTES)?;
        if self.client_timestamp_wall_us == 0 {
            return Err(ProjectionContractError::InvalidArgument(
                "client_timestamp_wall_us must be non-zero".to_string(),
            ));
        }
        Ok(())
    }
}

/// `list` request. Enumeration is caller-scoped rather than projection-scoped,
/// so it deliberately carries no projection ID or owner token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListProjectionsRequest {
    pub request_id: String,
    pub client_timestamp_wall_us: u64,
}

impl ListProjectionsRequest {
    pub fn validate(&self) -> Result<(), ProjectionContractError> {
        validate_non_empty_bounded("request_id", &self.request_id, crate::MAX_REQUEST_ID_BYTES)?;
        if self.client_timestamp_wall_us == 0 {
            return Err(ProjectionContractError::InvalidArgument(
                "client_timestamp_wall_us must be non-zero".to_string(),
            ));
        }
        Ok(())
    }
}

/// `attach` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRequest {
    #[serde(flatten)]
    pub envelope: OperationEnvelope,
    pub provider_kind: ProviderKind,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_profile_hint: Option<String>,
    #[serde(default)]
    pub content_classification: ContentClassification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hud_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl AttachRequest {
    pub fn validate(&self) -> Result<(), ProjectionContractError> {
        self.envelope.validate(ProjectionOperation::Attach)?;
        validate_non_empty_bounded(
            "display_name",
            &self.display_name,
            crate::MAX_DISPLAY_NAME_BYTES,
        )?;
        validate_optional_bounded(
            "workspace_hint",
            &self.workspace_hint,
            crate::MAX_HINT_BYTES,
        )?;
        validate_optional_bounded(
            "repository_hint",
            &self.repository_hint,
            crate::MAX_HINT_BYTES,
        )?;
        validate_optional_bounded(
            "icon_profile_hint",
            &self.icon_profile_hint,
            crate::MAX_HINT_BYTES,
        )?;
        validate_optional_bounded("hud_target", &self.hud_target, crate::MAX_HINT_BYTES)?;
        validate_optional_bounded(
            "idempotency_key",
            &self.idempotency_key,
            crate::MAX_REQUEST_ID_BYTES,
        )?;
        Ok(())
    }
}

/// `publish_output` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishOutputRequest {
    #[serde(flatten)]
    pub envelope: OperationEnvelope,
    pub owner_token: String,
    pub output_text: String,
    #[serde(default)]
    pub output_kind: OutputKind,
    #[serde(default)]
    pub content_classification: ContentClassification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_unit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coalesce_key: Option<String>,
    /// Optional question signal (a.k.a. `Question`): `true` marks this output
    /// as a question awaiting a viewer reply. Defaults to `false` — omitted is
    /// the exact pre-existing behavior (no cue rendered). Backward-compatible
    /// opt-in presence semantic (hud-jip0k).
    #[serde(default)]
    pub expects_reply: bool,
}

impl PublishOutputRequest {
    pub fn validate(&self, bounds: &ProjectionBounds) -> Result<(), ProjectionContractError> {
        self.envelope.validate(ProjectionOperation::PublishOutput)?;
        validate_owner_token(&self.owner_token)?;
        // §Viewer Reply Echo: the viewer turn kind is authored only by the
        // runtime's submit path; an adapter publish through this contract
        // MUST NOT be able to forge one. `tze_hud_runtime::portal_projection_driver
        // ::parse_output_kind` already string-rejects "viewer" before this
        // request is even constructed, but that is only one of several
        // callers of `handle_publish_output` — reject it here too so every
        // caller of this contract (including transports that skip the driver
        // shim) is covered.
        if self.output_kind == OutputKind::Viewer {
            return Err(ProjectionContractError::InvalidArgument(
                "output_kind \"viewer\" is reserved for viewer-submitted input \
                 and cannot be published by an agent"
                    .to_string(),
            ));
        }
        if self.output_text.len() > bounds.max_output_bytes_per_call {
            return Err(ProjectionContractError::StableCode(
                ProjectionErrorCode::ProjectionOutputTooLarge,
                "output_text exceeds max_output_bytes_per_call".to_string(),
            ));
        }
        validate_optional_bounded(
            "logical_unit_id",
            &self.logical_unit_id,
            crate::MAX_REQUEST_ID_BYTES,
        )?;
        validate_optional_bounded(
            "coalesce_key",
            &self.coalesce_key,
            crate::MAX_REQUEST_ID_BYTES,
        )?;
        Ok(())
    }
}

/// `publish_status` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishStatusRequest {
    #[serde(flatten)]
    pub envelope: OperationEnvelope,
    pub owner_token: String,
    pub lifecycle_state: ProjectionLifecycleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
}

impl PublishStatusRequest {
    pub fn validate(&self, bounds: &ProjectionBounds) -> Result<(), ProjectionContractError> {
        self.envelope.validate(ProjectionOperation::PublishStatus)?;
        validate_owner_token(&self.owner_token)?;
        if self
            .status_text
            .as_ref()
            .is_some_and(|text| text.len() > bounds.max_status_text_bytes)
        {
            return Err(ProjectionContractError::InvalidArgument(
                "status_text exceeds max_status_text_bytes".to_string(),
            ));
        }
        Ok(())
    }
}

/// `get_pending_input` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetPendingInputRequest {
    #[serde(flatten)]
    pub envelope: OperationEnvelope,
    pub owner_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}

impl GetPendingInputRequest {
    pub fn validate(&self) -> Result<(), ProjectionContractError> {
        self.envelope
            .validate(ProjectionOperation::GetPendingInput)?;
        validate_owner_token(&self.owner_token)
    }
}

/// `acknowledge_input` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcknowledgeInputRequest {
    #[serde(flatten)]
    pub envelope: OperationEnvelope,
    pub owner_token: String,
    pub input_id: String,
    pub ack_state: InputAckState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before_wall_us: Option<u64>,
}

impl AcknowledgeInputRequest {
    pub fn validate(&self) -> Result<(), ProjectionContractError> {
        self.envelope
            .validate(ProjectionOperation::AcknowledgeInput)?;
        validate_owner_token(&self.owner_token)?;
        validate_non_empty_bounded("input_id", &self.input_id, crate::MAX_REQUEST_ID_BYTES)?;
        validate_optional_bounded(
            "ack_message",
            &self.ack_message,
            crate::MAX_ACK_MESSAGE_BYTES,
        )?;
        if self.ack_state != InputAckState::Deferred && self.not_before_wall_us.is_some() {
            return Err(ProjectionContractError::InvalidArgument(
                "not_before_wall_us is valid only when ack_state is deferred".to_string(),
            ));
        }
        if self.ack_state == InputAckState::Deferred && self.not_before_wall_us == Some(0) {
            return Err(ProjectionContractError::InvalidArgument(
                "not_before_wall_us must be non-zero when present".to_string(),
            ));
        }
        Ok(())
    }
}

/// `detach` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachRequest {
    #[serde(flatten)]
    pub envelope: OperationEnvelope,
    pub owner_token: String,
    pub reason: String,
}

impl DetachRequest {
    pub fn validate(&self) -> Result<(), ProjectionContractError> {
        self.envelope.validate(ProjectionOperation::Detach)?;
        validate_owner_token(&self.owner_token)?;
        validate_non_empty_bounded("reason", &self.reason, crate::MAX_REASON_BYTES)
    }
}

/// Bounded pending input item returned by `get_pending_input`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingInputItem {
    pub input_id: String,
    pub projection_id: String,
    pub submission_text: String,
    pub submitted_at_wall_us: u64,
    pub expires_at_wall_us: u64,
    pub delivery_state: InputDeliveryState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at_wall_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before_wall_us: Option<u64>,
    #[serde(default)]
    pub content_classification: ContentClassification,
}

/// HUD-originated text submitted from an expanded projected portal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortalInputSubmission {
    pub input_id: String,
    pub submission_text: String,
    pub submitted_at_wall_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_wall_us: Option<u64>,
    #[serde(default)]
    pub content_classification: ContentClassification,
}

impl PortalInputSubmission {
    pub(super) fn effective_expires_at_wall_us(&self) -> Result<u64, ProjectionErrorCode> {
        if self.submitted_at_wall_us == 0 {
            return Err(ProjectionErrorCode::ProjectionInvalidArgument);
        }
        let expires_at_wall_us = self.expires_at_wall_us.map(Ok).unwrap_or_else(|| {
            self.submitted_at_wall_us
                .checked_add(crate::DEFAULT_PORTAL_INPUT_TTL_WALL_US)
                .ok_or(ProjectionErrorCode::ProjectionInvalidArgument)
        })?;
        if expires_at_wall_us <= self.submitted_at_wall_us {
            return Err(ProjectionErrorCode::ProjectionInvalidArgument);
        }
        Ok(expires_at_wall_us)
    }
}

/// Bounded local feedback returned after a HUD composer submission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortalInputFeedback {
    pub projection_id: String,
    pub input_id: String,
    pub feedback_state: PortalInputFeedbackState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ProjectionErrorCode>,
    pub pending_input_count: usize,
    pub pending_input_bytes: usize,
    pub status_summary: String,
}

/// Runtime session metadata retained by the projection authority while a HUD
/// connection is live. Lease use is authorized against these grants, not
/// against cached lease identity alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HudConnectionMetadata {
    pub connection_id: String,
    pub authenticated_session_id: String,
    #[serde(default)]
    pub granted_capabilities: Vec<String>,
    pub connected_at_wall_us: u64,
    pub last_reconnect_wall_us: u64,
}

impl HudConnectionMetadata {
    pub(super) fn validate(&self) -> Result<(), ProjectionContractError> {
        validate_non_empty_bounded("connection_id", &self.connection_id, crate::MAX_HINT_BYTES)?;
        validate_non_empty_bounded(
            "authenticated_session_id",
            &self.authenticated_session_id,
            crate::MAX_HINT_BYTES,
        )?;
        for capability in &self.granted_capabilities {
            validate_non_empty_bounded("granted_capability", capability, crate::MAX_HINT_BYTES)?;
        }
        if self.connected_at_wall_us == 0 || self.last_reconnect_wall_us == 0 {
            return Err(ProjectionContractError::InvalidArgument(
                "HUD connection timestamps must be non-zero".to_string(),
            ));
        }
        Ok(())
    }
}

/// Advisory runtime lease identity cached by the projection authority. This
/// value never authorizes republish by itself; callers must also have a fresh
/// authenticated HUD connection with grants covering requested capabilities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvisoryLeaseIdentity {
    pub lease_id: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub acquired_at_wall_us: u64,
    pub expires_at_wall_us: u64,
}

impl AdvisoryLeaseIdentity {
    pub(super) fn validate(&self) -> Result<(), ProjectionContractError> {
        validate_non_empty_bounded("lease_id", &self.lease_id, crate::MAX_HINT_BYTES)?;
        for capability in &self.capabilities {
            validate_non_empty_bounded("lease_capability", capability, crate::MAX_HINT_BYTES)?;
        }
        if self.acquired_at_wall_us == 0 || self.expires_at_wall_us == 0 {
            return Err(ProjectionContractError::InvalidArgument(
                "lease timestamps must be non-zero".to_string(),
            ));
        }
        if self.acquired_at_wall_us >= self.expires_at_wall_us {
            return Err(ProjectionContractError::InvalidArgument(
                "lease expiry must be after acquisition".to_string(),
            ));
        }
        Ok(())
    }
}

/// Reconnect counters retained while the projection authority process is alive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconnectBookkeeping {
    pub reconnect_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_disconnect_wall_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reconnect_wall_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_wall_us: Option<u64>,
}

/// One retained transcript logical unit. Text is memory-only v1 private state
/// and is purged when the owning projection is removed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptUnit {
    pub sequence: u64,
    pub output_text: String,
    pub output_kind: OutputKind,
    pub content_classification: ContentClassification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_unit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coalesce_key: Option<String>,
    /// `true` when this unit is a question awaiting a viewer reply
    /// (hud-jip0k). `#[serde(default)]` so retained/serialized units from
    /// before this field existed deserialize as `false` — the exact
    /// pre-existing behavior.
    #[serde(default)]
    pub expects_reply: bool,
    pub appended_at_wall_us: u64,
}

impl TranscriptUnit {
    pub(super) fn byte_len(&self) -> usize {
        self.output_text.len()
    }
}

/// Bounded portal update materialization returned to a daemon adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortalTranscriptUpdate {
    pub projection_id: String,
    pub visible_transcript: Vec<TranscriptUnit>,
    pub visible_transcript_bytes: usize,
    pub coalesced_output_count: usize,
    pub unread_output_count: usize,
    /// Wall-clock timestamp (µs) when the most-recently-coalesced pending append
    /// was submitted via `handle_publish_output`. Populated from
    /// `PortalCadenceCoalescer::peek_submitted_at` for arrival→present latency
    /// measurement (tasks.md §5.7, hud-zmt1a).
    ///
    /// Because the coalescer uses latest-wins semantics, this reflects the timestamp
    /// of the most recent accepted append, not the first one. If multiple appends
    /// are coalesced before the next portal update, this advances on each accepted
    /// append.
    ///
    /// Zero when no coalescer entry is found (e.g., direct `take_due_portal_update`
    /// call without going through `handle_publish_output`).
    pub submitted_at_us: u64,
}

/// Compact memory-only state summary that excludes transcript text, pending
/// input text, owner tokens, and lease credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionStateSummary {
    pub projection_id: String,
    pub lifecycle_state: ProjectionLifecycleState,
    pub content_classification: ContentClassification,
    pub has_hud_connection: bool,
    pub has_advisory_lease: bool,
    pub retained_transcript_bytes: usize,
    pub visible_transcript_bytes: usize,
    pub retained_transcript_units: usize,
    pub pending_input_count: usize,
    pub pending_input_bytes: usize,
    pub unread_output_count: usize,
    pub reconnect: ReconnectBookkeeping,
}

/// Bounded caller-visible metadata for one active projection.
///
/// This intentionally excludes transcript text, pending-input text, owner
/// tokens, classification, leases, and caller identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionListEntry {
    pub projection_id: String,
    pub display_name: String,
    pub lifecycle_state: ProjectionLifecycleState,
    pub unread_output_count: usize,
    pub pending_input_count: usize,
}

/// Common operation response envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionResponse {
    pub request_id: String,
    pub projection_id: String,
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ProjectionErrorCode>,
    pub server_timestamp_wall_us: u64,
    pub status_summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_state: Option<ProjectionLifecycleState>,
    /// Caller-scoped summaries returned only by the `list` operation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projections: Vec<ProjectionListEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_input: Vec<PendingInputItem>,
    #[serde(default)]
    pub pending_remaining_count: usize,
    #[serde(default)]
    pub pending_remaining_bytes: usize,
    #[serde(default)]
    pub portal_update_ready: bool,
    #[serde(default)]
    pub coalesced_output_count: usize,
}

impl ProjectionResponse {
    pub(super) fn accepted(
        request_id: impl Into<String>,
        projection_id: impl Into<String>,
        server_timestamp_wall_us: u64,
        status_summary: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            projection_id: projection_id.into(),
            accepted: true,
            error_code: None,
            server_timestamp_wall_us,
            status_summary: bounded_copy(status_summary.into(), crate::MAX_STATUS_SUMMARY_BYTES),
            owner_token: None,
            lifecycle_state: None,
            projections: Vec::new(),
            pending_input: Vec::new(),
            pending_remaining_count: 0,
            pending_remaining_bytes: 0,
            portal_update_ready: false,
            coalesced_output_count: 0,
        }
    }

    pub(super) fn denied(
        request_id: impl Into<String>,
        projection_id: impl Into<String>,
        server_timestamp_wall_us: u64,
        error_code: ProjectionErrorCode,
        status_summary: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            projection_id: projection_id.into(),
            accepted: false,
            error_code: Some(error_code),
            server_timestamp_wall_us,
            status_summary: bounded_copy(status_summary.into(), crate::MAX_STATUS_SUMMARY_BYTES),
            owner_token: None,
            lifecycle_state: None,
            projections: Vec::new(),
            pending_input: Vec::new(),
            pending_remaining_count: 0,
            pending_remaining_bytes: 0,
            portal_update_ready: false,
            coalesced_output_count: 0,
        }
    }
}

/// Bounded identity metadata for a live projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionIdentitySummary {
    pub provider_kind: ProviderKind,
    pub display_name: String,
    pub content_classification: ContentClassification,
    pub lifecycle_state: ProjectionLifecycleState,
}

/// Viewer and runtime policy applied while materializing projected portal
/// state. Defaults fail closed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedPortalPolicy {
    pub viewer_clearance: ContentClassification,
    pub reveal_identity: bool,
    pub reveal_lifecycle: bool,
    pub reveal_transcript: bool,
    pub reveal_unread: bool,
    pub reveal_pending_input: bool,
    pub allow_input: bool,
    pub safe_mode_active: bool,
    pub frozen: bool,
    pub dismissed: bool,
}

impl ProjectedPortalPolicy {
    /// Policy fixture that permits all private projected-session fields.
    pub fn permit_all() -> Self {
        Self {
            viewer_clearance: ContentClassification::Sensitive,
            reveal_identity: true,
            reveal_lifecycle: true,
            reveal_transcript: true,
            reveal_unread: true,
            reveal_pending_input: true,
            allow_input: true,
            safe_mode_active: false,
            frozen: false,
            dismissed: false,
        }
    }

    pub(super) fn permits(&self, classification: ContentClassification) -> bool {
        classification <= self.viewer_clearance
    }
}

impl Default for ProjectedPortalPolicy {
    fn default() -> Self {
        Self {
            viewer_clearance: ContentClassification::Public,
            reveal_identity: false,
            reveal_lifecycle: false,
            reveal_transcript: false,
            reveal_unread: false,
            reveal_pending_input: false,
            allow_input: false,
            safe_mode_active: false,
            frozen: false,
            dismissed: false,
        }
    }
}

/// Bounded state for resident-session text-stream portal materialization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedPortalState {
    pub projection_id: String,
    pub portal_id: String,
    pub adapter_family: ProjectedPortalAdapterFamily,
    pub runtime_authority: ProjectedPortalRuntimeAuthority,
    pub layer: ProjectedPortalLayer,
    pub presentation: ProjectedPortalPresentation,
    pub preserve_geometry: bool,
    pub redacted: bool,
    /// Content-free connection-degraded geometry signal (portal-disconnect-resume-ux
    /// §2/§3). `true` when the driving stream/session is degraded or HUD-unavailable.
    ///
    /// This is computed from the session lifecycle **independently of viewer
    /// redaction**, exactly like the scroll-position indicator: it conveys only
    /// connection state, never identity or transcript content, so a restricted
    /// viewer still sees that the portal is disconnected. The redaction-gated
    /// `lifecycle_state` field still spells `Degraded` vs `HudUnavailable` only
    /// to permitted viewers.
    #[serde(default)]
    pub connection_degraded: bool,
    /// Content-free "has the owning session ever connected" signal
    /// (portal-chat-grade-affordances §Connecting State Distinction). `true` once
    /// the driving session has established its first HUD connection (or has since
    /// disconnected — a portal that connected then dropped has still *ever*
    /// connected). `false` only for a freshly-attached, never-connected portal
    /// ("connecting").
    ///
    /// Like `connection_degraded`, this is computed from the session lifecycle
    /// **independently of viewer redaction**: it conveys connection state only,
    /// never identity or transcript content, so a restricted viewer still gets
    /// the correct first-run vs connecting distinction. The first-run empty
    /// treatment (hud-g1ena.6) yields to the connecting treatment when this is
    /// `false`, so a starting-up portal never renders as a "ready" empty state.
    #[serde(default)]
    pub has_ever_connected: bool,
    pub interaction_enabled: bool,
    pub attention: ProjectedPortalAttention,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_kind: Option<ProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_profile_hint: Option<String>,
    /// Optional HUD routing hint carried from the `attach` request, surfaced for
    /// introspection. Redaction-gated identically to the sibling identity hints
    /// (`None` when the viewer may not see attach identity). Inert in the current
    /// single-HUD deploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hud_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_state: Option<ProjectionLifecycleState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
    #[serde(default)]
    pub visible_transcript: Vec<TranscriptUnit>,
    pub visible_transcript_bytes: usize,
    /// The viewer's own accepted submissions — the INPUT history — held and
    /// projected SEPARATELY from `visible_transcript` (the OUTPUT/agent
    /// transcript). Per §Viewer Reply Echo the portal maintains two distinct,
    /// separately-bounded streams: a viewer echo lands here and never in
    /// `visible_transcript`, so a submission never grows `visible_transcript_bytes`
    /// nor moves the OUTPUT transcript's follow-tail scroll. Kind-distinct
    /// (`OutputKind::Viewer`), never unread, and redaction-gated identically to
    /// `visible_transcript` (emptied for a viewer whose clearance redacts the
    /// transcript). Bounded newest-fit on the append side.
    #[serde(default)]
    pub input_history: Vec<TranscriptUnit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unread_output_count: Option<usize>,
    /// Count of unread agent-authored outputs that survive this viewer's
    /// clearance filter — i.e. how many of the aggregate `unread_output_count`
    /// units actually appear in `visible_transcript`. Redaction-gated identically
    /// to `unread_output_count` (`None` when the count is not revealed).
    ///
    /// The in-transcript unread divider (hud-g1ena.2) is placed with THIS count,
    /// never the aggregate: a viewer whose clearance filters out a
    /// higher-classification unread turn must not have the divider pushed onto an
    /// already-seen visible turn (the aggregate would over-count the units below
    /// the divider). `unread_output_count` stays the aggregate for the ambient
    /// count, which MAY legitimately exceed the units below the divider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_unread_output_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_input_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_input_bytes: Option<usize>,
    /// Delivery state of the viewer's most recent submitted reply, surfaced so the
    /// portal can render an ambient per-turn delivery-acknowledgement cue on the
    /// echoed viewer turn (hud-g1ena.1, §Viewer Turn Delivery Acknowledgement).
    ///
    /// This is a **local presentation of state the runtime already owns**: it is
    /// derived from the authority's existing `pending_input` bookkeeping (the tail
    /// item — the newest viewer submission — and its evolving delivery state), so
    /// rendering the cue introduces **no new adapter round trip** and discloses no
    /// viewer read/seen state back to the adapter. `None` when there is no tracked
    /// viewer submission, or when the transcript is redacted for this viewer — the
    /// cue redacts together with the transcript it annotates.
    ///
    /// Phase-1 scope: the single-node raw-tile pilot surfaces the latest viewer
    /// turn's delivery state as one portal-level cue rather than a precise per-turn
    /// mark; precise per-turn placement is deferred to the promotion-era structured
    /// multi-node layout, exactly like the other Phase-1 ambient markers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_viewer_delivery_state: Option<InputDeliveryState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_input_feedback: Option<PortalInputFeedback>,
    /// Latest batched adapter draft notification. Present only when the
    /// composer is focused and a draft change has occurred since the last
    /// adapter delivery. The adapter consumes this batch instead of
    /// republishing the composer text on every keystroke.
    ///
    /// Spec §4.6: adapter consumes draft-state notifications rather than
    /// per-keystroke republish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_batch: Option<AdapterDraftBatch>,
    /// Latest batched portal geometry snapshot. Present only when geometry has
    /// changed since the last adapter delivery (pointer resize gesture or hotkey
    /// resize). The adapter MUST drop its own `publish_geometry` while
    /// `geometry_batch.latest.gesture_active == true`.
    ///
    /// Spec §6b.4: "gesture remains authoritative over adapter publishes until
    /// gesture end."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry_batch: Option<AdapterGeometryBatch>,
    /// Persistent resized portal bounds (hud-v4k1h follow-up). Unlike
    /// `geometry_batch` — a transient, consume-after-delivery notification to the
    /// owning adapter — this is the **durable** size the portal was last resized
    /// to (pointer gesture or Ctrl+= / Ctrl+- hotkey). The runtime updates the
    /// tile bounds locally on resize, but the rendered portal body + composer are
    /// sized from `bounds_for_state`; without a persistent override that body
    /// keeps re-rendering at the fixed config size, leaving an empty "shadow"
    /// region in the grown tile. `bounds_for_state` prefers this (Expanded only)
    /// so the body follows the resize. `None` until the first resize occurs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resized_bounds: Option<AdapterPortalRect>,
}

// ─── Composer draft notification types (hud-5jbra.4) ─────────────────────────

/// Adapter-facing coalescible draft-state notification (state-stream class).
///
/// Delivered to the owning adapter to replace per-keystroke composer-text
/// republish. The adapter MAY receive a single latest-snapshot rather than
/// per-keystroke events; coalescing is the caller's responsibility.
///
/// Spec: §4.3 — "state-stream traffic, coalescible to the latest draft snapshot."
/// Spec: §4.6 — "update the cooperative projection adapter … to consume
/// draft-state notifications instead of per-keystroke republish."
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterDraftNotification {
    /// Current draft text (never exceeds the configured draft byte cap).
    pub text: String,
    /// Cursor byte offset into `text`.
    pub cursor: usize,
    /// Selection anchor byte offset. Equal to `cursor` when no selection.
    pub selection_anchor: usize,
    /// True when the draft is at or over its byte cap.
    pub at_capacity: bool,
    /// Monotonic sequence from the runtime draft buffer.
    pub sequence: u64,
}

/// Adapter-facing transactional draft submission.
///
/// Delivered exactly once when the viewer submits the draft. The submitted
/// text equals the local buffer at the moment of submission; it is the
/// authoritative content to forward to the owning adapter's semantic inbox.
///
/// Spec: §4.3 — "submission and cancel SHALL remain transactional."
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterDraftSubmission {
    /// Submitted text (equals local buffer at submit time).
    pub text: String,
    /// Sequence at submit time.
    pub sequence: u64,
}

/// Adapter-facing transactional draft cancel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterDraftCancel {
    pub sequence: u64,
}

/// Batched adapter notification for state-stream coalescing.
///
/// The adapter consumes `latest` (latest-wins) for real-time display of draft
/// state, and `submission` / `cancel` for transactional handling. Older
/// state-stream entries within the same batch window are discarded.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AdapterDraftBatch {
    /// Latest draft snapshot (coalescible — newer replaces older).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<AdapterDraftNotification>,
    /// Pending transactional submission (first wins; not coalescible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission: Option<AdapterDraftSubmission>,
    /// Pending transactional cancel (first wins; not coalescible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<AdapterDraftCancel>,
}

impl AdapterDraftBatch {
    /// Create an empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Coalesce a state-stream notification (latest-wins).
    pub fn coalesce_state(&mut self, notification: AdapterDraftNotification) {
        match &self.latest {
            Some(existing) if notification.sequence > existing.sequence => {
                self.latest = Some(notification);
            }
            None => {
                self.latest = Some(notification);
            }
            _ => {}
        }
    }

    /// Record a transactional submission (first wins; clears any pending cancel
    /// to enforce submit-XOR-cancel semantics).
    pub fn record_submission(&mut self, sub: AdapterDraftSubmission) {
        if self.submission.is_none() {
            self.cancel = None;
            self.submission = Some(sub);
        }
    }

    /// Record a transactional cancel (first wins; clears any pending submission
    /// to enforce submit-XOR-cancel semantics).
    pub fn record_cancel(&mut self, cancel: AdapterDraftCancel) {
        if self.cancel.is_none() {
            self.submission = None;
            self.cancel = Some(cancel);
        }
    }

    /// True if the batch holds nothing to deliver (no latest snapshot, no
    /// submission, no cancel).
    ///
    /// Twin: `tze_hud_input::composer_draft::DraftNotificationBatch::is_empty`
    /// is the cross-crate source type this is a clone of. Keep the two in sync.
    pub fn is_empty(&self) -> bool {
        self.latest.is_none() && self.submission.is_none() && self.cancel.is_none()
    }
}

// ─── Portal geometry update types (hud-5jbra.9) ──────────────────────────────

/// Adapter-facing portal bounding rectangle in integer display pixels.
///
/// Coordinates are rounded from display-space f32 values (nearest integer) so
/// that this type derives `Eq` and plays well with `ProjectedPortalState`'s
/// existing `Eq` bound. The rendering layer converts back to f32 before issuing
/// draw commands. Sub-pixel precision is intentionally dropped at the adapter
/// boundary; only the compositor's internal f32 geometry is sub-pixel exact.
///
/// ## Coalescible (state-stream)
///
/// `AdapterGeometrySnapshot` is a state-stream payload. The transport MUST
/// deliver only the latest snapshot per delivery window (latest-wins), not
/// every intermediate geometry during a gesture.
///
/// Spec §6b.4: "geometry changes [are delivered] to the owning adapter as
/// coalescible state-stream snapshots."
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterPortalRect {
    /// Left edge of the portal in display pixels (rounded from f32).
    pub x_px: i32,
    /// Top edge of the portal in display pixels (rounded from f32).
    pub y_px: i32,
    /// Width of the portal in display pixels (rounded from f32, ≥ 0).
    pub width_px: i32,
    /// Height of the portal in display pixels (rounded from f32, ≥ 0).
    pub height_px: i32,
}

impl AdapterPortalRect {
    /// Construct from float display-space coordinates.
    ///
    /// Width and height are clamped to 0 before rounding to avoid negative
    /// values from rounding artefacts at the minimum-clamped boundary.
    pub fn from_f32(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x_px: x.round() as i32,
            y_px: y.round() as i32,
            width_px: width.max(0.0).round() as i32,
            height_px: height.max(0.0).round() as i32,
        }
    }
}

/// Coalescible adapter-facing geometry snapshot (§6b.4).
///
/// Delivered to the owning adapter when portal geometry changes due to a
/// pointer gesture or hotkey resize. The adapter MUST NOT apply `publish_geometry`
/// while `gesture_active == true` — gesture snapshots are authoritative.
///
/// Message class: **state-stream**. Older snapshots for the same portal MUST be
/// discarded when a newer one arrives within the same adapter delivery window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterGeometrySnapshot {
    /// Final clamped portal bounds at this gesture step.
    pub rect: AdapterPortalRect,
    /// True while a pointer gesture is active (gesture is authoritative).
    /// False on hotkey resize or gesture end (adapter may resume publishing).
    pub gesture_active: bool,
    /// Monotonic sequence counter — allows the adapter to detect skipped
    /// snapshots when the transport does not deliver every event.
    pub sequence: u64,
}

/// Batched adapter geometry notification for state-stream coalescing (§6b.4).
///
/// Replaces the `latest` field on each new snapshot (latest-wins). The adapter
/// always reads `latest` if present to update portal geometry.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AdapterGeometryBatch {
    /// Latest geometry snapshot (coalescible — newer replaces older).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<AdapterGeometrySnapshot>,
}

impl AdapterGeometryBatch {
    /// Coalesce a geometry snapshot (latest-wins by sequence number).
    pub fn coalesce(&mut self, snapshot: AdapterGeometrySnapshot) {
        match &self.latest {
            Some(existing) if snapshot.sequence > existing.sequence => {
                self.latest = Some(snapshot);
            }
            None => {
                self.latest = Some(snapshot);
            }
            _ => {}
        }
    }

    /// True if there is a snapshot to deliver.
    pub fn is_empty(&self) -> bool {
        self.latest.is_none()
    }
}

/// Errors raised by schema validation or token generation.
#[derive(Debug, Error)]
pub enum ProjectionContractError {
    #[error("invalid projection argument: {0}")]
    InvalidArgument(String),
    #[error("{0}: {1}")]
    StableCode(ProjectionErrorCode, String),
    #[error("unable to generate owner token")]
    TokenGeneration,
}

impl ProjectionContractError {
    pub(super) fn code(&self) -> ProjectionErrorCode {
        match self {
            Self::InvalidArgument(_) => ProjectionErrorCode::ProjectionInvalidArgument,
            Self::StableCode(code, _) => *code,
            Self::TokenGeneration => ProjectionErrorCode::ProjectionInternalError,
        }
    }
}

pub(super) fn validate_owner_token(owner_token: &str) -> Result<(), ProjectionContractError> {
    validate_non_empty_bounded(
        "owner_token",
        owner_token,
        crate::OWNER_TOKEN_ENTROPY_BITS / 4,
    )
}

pub(crate) fn validate_non_empty_bounded(
    field: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ProjectionContractError> {
    if value.is_empty() {
        return Err(ProjectionContractError::InvalidArgument(format!(
            "{field} is required"
        )));
    }
    if value.len() > max_bytes {
        return Err(ProjectionContractError::InvalidArgument(format!(
            "{field} exceeds {max_bytes} bytes"
        )));
    }
    Ok(())
}

pub(crate) fn validate_optional_bounded(
    field: &str,
    value: &Option<String>,
    max_bytes: usize,
) -> Result<(), ProjectionContractError> {
    if let Some(value) = value {
        validate_non_empty_bounded(field, value, max_bytes)?;
    }
    Ok(())
}

pub(super) fn generate_owner_token() -> Result<String, ProjectionContractError> {
    let mut token_bytes = [0u8; crate::OWNER_TOKEN_ENTROPY_BITS / 8];
    getrandom::fill(&mut token_bytes).map_err(|_| ProjectionContractError::TokenGeneration)?;
    Ok(hex_encode(&token_bytes))
}

pub(super) fn verifier_for_secret(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

pub(super) fn constant_time_eq(left: &str, right: &str) -> bool {
    left.as_bytes().ct_eq(right.as_bytes()).into()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn bounded_copy(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}
