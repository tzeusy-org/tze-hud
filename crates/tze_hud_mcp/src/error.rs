//! MCP error types.
//!
//! Two layers:
//!
//! - **Tool errors** ([`McpError::Tool`]): every failure of a `tools/call`.
//!   They come back as a tool result with `isError: true` and one text block
//!   holding `{"code":"...","hint":"..."}`. `code` is from the closed set
//!   [`ERROR_CODES`] (shared with gRPC, documented in `docs/api.md`); `hint`
//!   names the next call.
//! - **Protocol errors** ([`JsonRpcError`]): the request itself is unusable
//!   (bad JSON, unknown method or tool, missing auth). Standard JSON-RPC 2.0
//!   codes.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tze_hud_projection::ProjectionErrorCode;

/// JSON-RPC 2.0 protocol error codes.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Missing or unknown PSK.
    pub const UNAUTHENTICATED: i64 = -32004;
}

/// The closed set of tool error codes (invariant 8). `docs/api.md` lists
/// each with its meaning; a test keeps the two in sync.
pub const ERROR_CODES: &[&str] = &[
    "INVALID_ARGUMENT",
    "NOT_ALLOWED",
    "NOT_HELD",
    "ZONE_NOT_FOUND",
    "WIDGET_NOT_FOUND",
    "WIDGET_PARAMETER_INVALID",
    "CONTENT_REJECTED",
    "LEASE_NOT_ACTIVE",
    "SAFE_MODE_ACTIVE",
    "TIMESTAMP_TOO_FUTURE",
    "UNAVAILABLE",
    "INTERNAL",
    "PROJECTION_NOT_FOUND",
    "PROJECTION_ALREADY_ATTACHED",
    "PROJECTION_UNAUTHORIZED",
    "PROJECTION_TOKEN_EXPIRED",
    "PROJECTION_INVALID_ARGUMENT",
    "PROJECTION_OUTPUT_TOO_LARGE",
    "PROJECTION_INPUT_TOO_LARGE",
    "PROJECTION_INPUT_QUEUE_FULL",
    "PROJECTION_RATE_LIMITED",
    "PROJECTION_STATE_CONFLICT",
    "PROJECTION_HUD_UNAVAILABLE",
    "PROJECTION_INTERNAL_ERROR",
];

/// A JSON-RPC 2.0 protocol error object.
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

    pub fn unauthenticated() -> Self {
        Self::new(codes::UNAUTHENTICATED, "Authentication required")
    }
}

/// A failed tool call.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum McpError {
    /// A stable code from [`ERROR_CODES`] plus a hint naming the next call.
    #[error("{code}: {hint}")]
    Tool { code: &'static str, hint: String },
}

impl McpError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Tool { code, .. } => code,
        }
    }

    /// The `{code, hint}` JSON carried in the error result's text block.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Tool { code, hint } => serde_json::json!({ "code": code, "hint": hint }),
        }
    }
}

/// The hint for a projection rejection surfaced through `hud_*` verbs.
pub const fn projection_hint(error_code: ProjectionErrorCode) -> &'static str {
    match error_code {
        ProjectionErrorCode::ProjectionNotFound
        | ProjectionErrorCode::ProjectionUnauthorized
        | ProjectionErrorCode::ProjectionTokenExpired => "hud_publish to the portal to re-attach",
        ProjectionErrorCode::ProjectionAlreadyAttached => {
            "another agent holds this portal id; pick another"
        }
        ProjectionErrorCode::ProjectionInvalidArgument => "fix the arguments and retry",
        ProjectionErrorCode::ProjectionOutputTooLarge => "split the output into smaller publishes",
        ProjectionErrorCode::ProjectionInputTooLarge => "reduce the input and retry",
        ProjectionErrorCode::ProjectionInputQueueFull => "hud_input with ack to drain input",
        ProjectionErrorCode::ProjectionRateLimited => "back off, then retry",
        ProjectionErrorCode::ProjectionStateConflict => "check state in hud_surfaces, then retry",
        ProjectionErrorCode::ProjectionHudUnavailable => "the HUD is unavailable; retry later",
        ProjectionErrorCode::ProjectionInternalError => "retry once",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_PROJECTION_CODES: [ProjectionErrorCode; 12] = [
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

    #[test]
    fn every_projection_code_is_in_the_closed_set() {
        for code in ALL_PROJECTION_CODES {
            assert!(ERROR_CODES.contains(&code.as_str()), "{}", code.as_str());
            assert!(!projection_hint(code).is_empty());
        }
    }

    #[test]
    fn error_codes_are_unique_and_documented() {
        let mut seen = std::collections::HashSet::new();
        for code in ERROR_CODES {
            assert!(seen.insert(code), "duplicate {code}");
        }
        let api = include_str!("../../../docs/api.md");
        for code in ERROR_CODES {
            assert!(
                api.contains(&format!("`{code}`")),
                "{code} missing from docs/api.md"
            );
        }
    }

    #[test]
    fn tool_error_json_is_code_and_hint() {
        let e = McpError::Tool {
            code: "ZONE_NOT_FOUND",
            hint: "call hud_surfaces".into(),
        };
        let v = e.to_json();
        assert_eq!(v["code"], "ZONE_NOT_FOUND");
        assert_eq!(v["hint"], "call hud_surfaces");
        assert_eq!(
            v.as_object().unwrap().len(),
            2,
            "one shape: code + hint only"
        );
    }
}
