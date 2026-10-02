//! MCP error types.
//!
//! JSON-RPC 2.0 error codes follow the spec:
//! - Parse error:      -32700
//! - Invalid request:  -32600
//! - Method not found: -32601
//! - Invalid params:   -32602
//! - Internal error:   -32603
//!
//! Application-level errors use -32000 and below.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tze_hud_projection::ProjectionErrorCode;

/// Standard JSON-RPC 2.0 error codes.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal error — also used for capability-required denials per spec §8.3.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Scene validation error (e.g. tab not found, lease expired).
    pub const SCENE_ERROR: i64 = -32000;
    /// The requested zone does not exist.
    pub const ZONE_NOT_FOUND: i64 = -32001;
    /// No active tab in the scene when one is required.
    pub const NO_ACTIVE_TAB: i64 = -32002;
    /// Invalid ID format (e.g. malformed UUID).
    pub const INVALID_ID: i64 = -32003;
    /// Authentication failed (bad or missing pre-shared key).
    pub const UNAUTHENTICATED: i64 = -32004;
    /// Stable append-only portal projection application errors.
    pub const PROJECTION_NOT_FOUND: i64 = -32100;
    pub const PROJECTION_ALREADY_ATTACHED: i64 = -32101;
    pub const PROJECTION_UNAUTHORIZED: i64 = -32102;
    pub const PROJECTION_TOKEN_EXPIRED: i64 = -32103;
    pub const PROJECTION_INVALID_ARGUMENT: i64 = -32104;
    pub const PROJECTION_OUTPUT_TOO_LARGE: i64 = -32105;
    pub const PROJECTION_INPUT_TOO_LARGE: i64 = -32106;
    pub const PROJECTION_INPUT_QUEUE_FULL: i64 = -32107;
    pub const PROJECTION_RATE_LIMITED: i64 = -32108;
    pub const PROJECTION_STATE_CONFLICT: i64 = -32109;
    pub const PROJECTION_HUD_UNAVAILABLE: i64 = -32110;
    pub const PROJECTION_INTERNAL_ERROR: i64 = -32111;
}

/// A serializable JSON-RPC 2.0 error object.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn parse_error() -> Self {
        Self::new(codes::PARSE_ERROR, "Parse error")
    }

    pub fn invalid_request() -> Self {
        Self::new(codes::INVALID_REQUEST, "Invalid Request")
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            codes::METHOD_NOT_FOUND,
            format!("Method not found: {method}"),
        )
    }

    pub fn invalid_params(reason: impl Into<String>) -> Self {
        Self::new(codes::INVALID_PARAMS, reason.into())
    }

    pub fn internal(reason: impl Into<String>) -> Self {
        Self::new(codes::INTERNAL_ERROR, reason.into())
    }

    pub fn scene_error(reason: impl Into<String>) -> Self {
        Self::new(codes::SCENE_ERROR, reason.into())
    }

    pub fn zone_not_found(name: &str) -> Self {
        Self::new(codes::ZONE_NOT_FOUND, format!("Zone not found: {name}"))
    }

    pub fn no_active_tab() -> Self {
        Self::new(codes::NO_ACTIVE_TAB, "No active tab in scene")
    }

    pub fn invalid_id(reason: impl Into<String>) -> Self {
        Self::new(codes::INVALID_ID, reason.into())
    }

    pub fn unauthenticated() -> Self {
        Self::new(codes::UNAUTHENTICATED, "Authentication required")
    }

    /// Projection authority rejection with a distinct append-only application
    /// code and bounded, non-secret recovery data (hud-w2h5c).
    ///
    /// Authority rejection details are deliberately not accepted here: they may
    /// contain owner-token or private-content context. The wire message and hint
    /// are fixed solely by the stable error code, and the operation is reduced to
    /// the known portal tool vocabulary before serialization.
    pub fn projection_rejected(error_code: ProjectionErrorCode, operation: &str) -> Self {
        let operation = bounded_projection_operation(operation);
        let recovery_operation = projection_recovery_operation(error_code, operation);
        let message = projection_error_message(error_code, operation, recovery_operation);
        let data = serde_json::json!({
            "error_code": error_code.as_str(),
            "message": message.clone(),
            "context": {
                "operation": operation,
                "subsystem": "portal_projection"
            },
            "hint": {
                "recovery_operation": recovery_operation,
                "resolution": projection_resolution(error_code)
            }
        });
        Self::new(projection_json_rpc_code(error_code), message).with_data(data)
    }

    /// Capability-required error per spec §8.3.
    ///
    /// Returns a JSON-RPC 2.0 error with:
    /// - code: -32603 (Internal error, as mandated by spec §8.3)
    /// - data.error_code: "CAPABILITY_REQUIRED"
    /// - data.context: "tool=<tool_name>"
    /// - data.hint: {"required_capability": "resident_mcp", "resolution": "..."}
    pub fn not_allowed(tool_name: &str, agent_id: &str, allow_entry: &str) -> Self {
        let data = serde_json::json!({
            "error_code": "NOT_ALLOWED",
            "message": "Not allowed",
            "context": format!("tool={tool_name} agent={agent_id}"),
            "hint": format!(
                "add \"{allow_entry}\" to allow in [agents.{agent_id}] of the runtime config"
            ),
        });
        Self::new(codes::INTERNAL_ERROR, "Not allowed").with_data(data)
    }
}

/// High-level MCP error for internal use. Converts to [`JsonRpcError`] for the wire.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("parse error: {0}")]
    ParseError(#[from] serde_json::Error),

    #[error("invalid params: {0}")]
    InvalidParams(String),

    #[error("scene error: {0}")]
    SceneError(String),

    #[error("zone not found: {0}")]
    ZoneNotFound(String),

    #[error("no active tab")]
    NoActiveTab,

    #[error("invalid id: {0}")]
    InvalidId(String),

    #[error("method not found: {0}")]
    MethodNotFound(String),

    #[error("internal error: {0}")]
    Internal(String),

    /// Authentication failed: bad or missing pre-shared key (spec §8.4).
    #[error("authentication required")]
    Unauthenticated,

    /// Projection authority rejected a `portal_projection_*` operation.
    /// Carries the stable [`ProjectionErrorCode`] so it reaches the wire as
    /// `data.error_code` instead of flattening to an opaque `-32603` message.
    #[error("projection rejected ({error_code}) during {operation}")]
    ProjectionRejected {
        /// Stable `PROJECTION_*` code surfaced to the LLM.
        error_code: ProjectionErrorCode,
        /// Fixed portal tool name; authority-provided details are discarded.
        operation: &'static str,
    },
}

impl From<McpError> for JsonRpcError {
    fn from(e: McpError) -> Self {
        match e {
            McpError::ParseError(inner) => {
                JsonRpcError::parse_error().with_data(serde_json::json!(inner.to_string()))
            }
            McpError::InvalidParams(msg) => JsonRpcError::invalid_params(msg),
            McpError::SceneError(msg) => JsonRpcError::scene_error(msg),
            McpError::ZoneNotFound(name) => JsonRpcError::zone_not_found(&name),
            McpError::NoActiveTab => JsonRpcError::no_active_tab(),
            McpError::InvalidId(msg) => JsonRpcError::invalid_id(msg),
            McpError::MethodNotFound(method) => JsonRpcError::method_not_found(&method),
            McpError::Internal(msg) => JsonRpcError::internal(msg),
            McpError::Unauthenticated => JsonRpcError::unauthenticated(),
            McpError::ProjectionRejected {
                error_code,
                operation,
            } => JsonRpcError::projection_rejected(error_code, operation),
        }
    }
}

const fn projection_json_rpc_code(error_code: ProjectionErrorCode) -> i64 {
    match error_code {
        ProjectionErrorCode::ProjectionNotFound => codes::PROJECTION_NOT_FOUND,
        ProjectionErrorCode::ProjectionAlreadyAttached => codes::PROJECTION_ALREADY_ATTACHED,
        ProjectionErrorCode::ProjectionUnauthorized => codes::PROJECTION_UNAUTHORIZED,
        ProjectionErrorCode::ProjectionTokenExpired => codes::PROJECTION_TOKEN_EXPIRED,
        ProjectionErrorCode::ProjectionInvalidArgument => codes::PROJECTION_INVALID_ARGUMENT,
        ProjectionErrorCode::ProjectionOutputTooLarge => codes::PROJECTION_OUTPUT_TOO_LARGE,
        ProjectionErrorCode::ProjectionInputTooLarge => codes::PROJECTION_INPUT_TOO_LARGE,
        ProjectionErrorCode::ProjectionInputQueueFull => codes::PROJECTION_INPUT_QUEUE_FULL,
        ProjectionErrorCode::ProjectionRateLimited => codes::PROJECTION_RATE_LIMITED,
        ProjectionErrorCode::ProjectionStateConflict => codes::PROJECTION_STATE_CONFLICT,
        ProjectionErrorCode::ProjectionHudUnavailable => codes::PROJECTION_HUD_UNAVAILABLE,
        ProjectionErrorCode::ProjectionInternalError => codes::PROJECTION_INTERNAL_ERROR,
    }
}

fn projection_error_message(
    error_code: ProjectionErrorCode,
    operation: &'static str,
    recovery_operation: &'static str,
) -> String {
    match error_code {
        ProjectionErrorCode::ProjectionNotFound => {
            format!("Projection not found; call {recovery_operation}, then retry {operation}.")
        }
        ProjectionErrorCode::ProjectionAlreadyAttached => format!(
            "Projection already attached; call {recovery_operation} with the original idempotency_key to rotate the owner token."
        ),
        ProjectionErrorCode::ProjectionUnauthorized => format!(
            "Projection owner authentication failed; call {recovery_operation} with the original idempotency_key to rotate the owner token."
        ),
        ProjectionErrorCode::ProjectionTokenExpired => format!(
            "Projection owner token expired; call {recovery_operation} with the original idempotency_key to rotate the owner token."
        ),
        ProjectionErrorCode::ProjectionInvalidArgument => format!(
            "Projection arguments are invalid; correct them, then retry {recovery_operation}."
        ),
        ProjectionErrorCode::ProjectionOutputTooLarge => format!(
            "Projection output exceeds its limit; split it, then retry {recovery_operation}."
        ),
        ProjectionErrorCode::ProjectionInputTooLarge => format!(
            "Projection input exceeds its limit; reduce it, then retry {recovery_operation}."
        ),
        ProjectionErrorCode::ProjectionInputQueueFull => format!(
            "Projection input queue is full; call {recovery_operation}, acknowledge pending input, then retry {operation}."
        ),
        ProjectionErrorCode::ProjectionRateLimited => format!(
            "Projection operation is rate limited; back off, then retry {recovery_operation}."
        ),
        ProjectionErrorCode::ProjectionStateConflict => {
            format!(
                "Projection state conflicts with this operation; refresh lifecycle state, then retry {recovery_operation}."
            )
        }
        ProjectionErrorCode::ProjectionHudUnavailable => format!(
            "HUD projection service is unavailable; wait for availability, then retry {recovery_operation}."
        ),
        ProjectionErrorCode::ProjectionInternalError => format!(
            "Projection operation failed internally; retry {recovery_operation} once, then inspect bounded runtime telemetry."
        ),
    }
}

const fn projection_resolution(error_code: ProjectionErrorCode) -> &'static str {
    match error_code {
        ProjectionErrorCode::ProjectionNotFound => {
            "Attach the projection, then retry the requested operation."
        }
        ProjectionErrorCode::ProjectionAlreadyAttached => {
            "Reuse the existing owner token or repeat the idempotent attach."
        }
        ProjectionErrorCode::ProjectionUnauthorized
        | ProjectionErrorCode::ProjectionTokenExpired => {
            "Perform an authenticated attach with the original idempotency_key to rotate the owner token, then retry."
        }
        ProjectionErrorCode::ProjectionInvalidArgument => {
            "Correct the documented arguments and retry the requested operation."
        }
        ProjectionErrorCode::ProjectionOutputTooLarge => {
            "Split the output into smaller bounded publishes and retry."
        }
        ProjectionErrorCode::ProjectionInputTooLarge => {
            "Reduce the input payload or raise the documented bounded input limit, then retry."
        }
        ProjectionErrorCode::ProjectionInputQueueFull => {
            "Drain and acknowledge pending input before retrying."
        }
        ProjectionErrorCode::ProjectionRateLimited => {
            "Back off for the configured rate window, then retry."
        }
        ProjectionErrorCode::ProjectionStateConflict => {
            "Refresh projection state, resolve the lifecycle conflict, then retry."
        }
        ProjectionErrorCode::ProjectionHudUnavailable => {
            "Wait for HUD availability and retry without recreating private content."
        }
        ProjectionErrorCode::ProjectionInternalError => {
            "Retry once; if it recurs, inspect bounded runtime telemetry using the stable error code."
        }
    }
}

const fn projection_recovery_operation(
    error_code: ProjectionErrorCode,
    operation: &'static str,
) -> &'static str {
    match error_code {
        ProjectionErrorCode::ProjectionNotFound
        | ProjectionErrorCode::ProjectionAlreadyAttached
        | ProjectionErrorCode::ProjectionUnauthorized
        | ProjectionErrorCode::ProjectionTokenExpired => "portal_projection_attach",
        ProjectionErrorCode::ProjectionInputQueueFull => "portal_projection_get_pending_input",
        _ => operation,
    }
}

fn bounded_projection_operation(operation: &str) -> &'static str {
    match operation {
        "portal_projection_list" => "portal_projection_list",
        "portal_projection_attach" => "portal_projection_attach",
        "portal_projection_publish" => "portal_projection_publish",
        "portal_projection_publish_status" => "portal_projection_publish_status",
        "portal_projection_get_pending_input" => "portal_projection_get_pending_input",
        "portal_projection_acknowledge_input" => "portal_projection_acknowledge_input",
        "portal_projection_detach" => "portal_projection_detach",
        "portal_projection_cleanup" => "portal_projection_cleanup",
        _ => "portal_projection_operation",
    }
}

impl From<tze_hud_scene::ValidationError> for McpError {
    fn from(e: tze_hud_scene::ValidationError) -> Self {
        McpError::SceneError(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tze_hud_projection::INITIAL_ERROR_CODES;

    #[test]
    fn projection_error_mapping_is_total_distinct_and_deterministic() {
        let expected_json_rpc_codes = [
            -32100, -32101, -32102, -32103, -32104, -32105, -32106, -32107, -32108, -32109, -32110,
            -32111,
        ];
        let mut observed_codes = HashSet::new();

        for (error_code, expected_json_rpc_code) in
            INITIAL_ERROR_CODES.into_iter().zip(expected_json_rpc_codes)
        {
            let first = JsonRpcError::projection_rejected(error_code, "portal_projection_publish");
            let second = JsonRpcError::projection_rejected(error_code, "portal_projection_publish");

            assert_eq!(first.code, expected_json_rpc_code, "{error_code}");
            assert!(
                observed_codes.insert(first.code),
                "{error_code} reused JSON-RPC code {}",
                first.code
            );
            assert_eq!(first.code, second.code, "{error_code}");
            assert_eq!(first.message, second.message, "{error_code}");
            assert!(
                !first.message.is_empty() && first.message.len() <= 160,
                "{error_code} message must be bounded and self-describing: {:?}",
                first.message
            );

            let data = first.data.expect("projection errors carry structured data");
            assert_eq!(data["error_code"], error_code.as_str(), "{error_code}");
            assert_eq!(
                data["context"]["operation"], "portal_projection_publish",
                "{error_code} must name the failed operation"
            );
            assert_eq!(data["context"]["subsystem"], "portal_projection");
            assert!(
                data["hint"]["recovery_operation"].is_string(),
                "{error_code} must name a recovery operation"
            );
            assert!(
                first.message.contains(
                    data["hint"]["recovery_operation"]
                        .as_str()
                        .expect("recovery operation is text")
                ),
                "{error_code} message must name its recovery operation: {:?}",
                first.message
            );
            assert!(
                data["hint"]["resolution"].is_string(),
                "{error_code} must prescribe a resolution"
            );
            assert!(
                data.to_string().len() <= 1_024,
                "{error_code} structured recovery data must stay bounded"
            );
            assert!(
                data["hint"]["resolution"]
                    .as_str()
                    .is_some_and(|resolution| resolution.len() <= 240),
                "{error_code} resolution must stay concise"
            );
        }

        assert_eq!(observed_codes.len(), INITIAL_ERROR_CODES.len());
    }

    #[test]
    fn owner_token_rejections_prescribe_authenticated_idempotent_reattach() {
        for error_code in [
            ProjectionErrorCode::ProjectionUnauthorized,
            ProjectionErrorCode::ProjectionTokenExpired,
        ] {
            let wire = JsonRpcError::projection_rejected(
                error_code,
                "portal_projection_get_pending_input",
            );
            assert_ne!(wire.code, codes::INTERNAL_ERROR, "{error_code}");
            let data = wire.data.expect("owner-token rejection has recovery data");
            assert_eq!(
                data["hint"]["recovery_operation"],
                "portal_projection_attach"
            );
            let resolution = data["hint"]["resolution"]
                .as_str()
                .expect("resolution is text");
            assert!(resolution.contains("authenticated"), "{resolution}");
            assert!(
                resolution.contains("original idempotency_key"),
                "{resolution}"
            );
            assert!(
                resolution.contains("rotate the owner token"),
                "{resolution}"
            );
        }
    }

    #[test]
    fn projection_error_context_accepts_only_bounded_operation_vocabulary() {
        let private_detail = "secret-token/private-output/".repeat(1_000);
        let wire = JsonRpcError::projection_rejected(
            ProjectionErrorCode::ProjectionUnauthorized,
            &private_detail,
        );
        let serialized = serde_json::to_string(&wire).expect("wire error serializes");

        assert!(!serialized.contains("secret-token"));
        assert!(!serialized.contains("private-output"));
        assert_eq!(
            wire.data.expect("structured data")["context"]["operation"],
            "portal_projection_operation"
        );
        assert!(serialized.len() <= 1_024);
    }
}
