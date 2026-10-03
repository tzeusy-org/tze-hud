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

/// The closed set of tool error codes (invariant 8), shared with gRPC.
/// `docs/api.md` lists each with its meaning; a test keeps the two in sync.
pub use tze_hud_scene::error_codes::ERROR_CODES;

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

/// Map a portal authority rejection to a shared code and a hint naming the
/// next call. The portal's own codes never reach the model.
pub const fn map_projection(error_code: ProjectionErrorCode) -> (&'static str, &'static str) {
    use ProjectionErrorCode as P;
    match error_code {
        P::ProjectionNotFound | P::ProjectionUnauthorized | P::ProjectionTokenExpired => {
            ("NOT_HELD", "hud_publish to the portal to re-attach")
        }
        P::ProjectionAlreadyAttached => (
            "NOT_ALLOWED",
            "another agent holds this portal id; pick another id",
        ),
        P::ProjectionInvalidArgument | P::ProjectionStateConflict => {
            ("INVALID_ARGUMENT", "fix the arguments and retry")
        }
        P::ProjectionOutputTooLarge => (
            "CONTENT_REJECTED",
            "split the output into smaller publishes",
        ),
        P::ProjectionInputTooLarge => ("CONTENT_REJECTED", "reduce the input and retry"),
        P::ProjectionInputQueueFull => ("BUDGET_EXCEEDED", "hud_input with ack to drain input"),
        P::ProjectionRateLimited => ("BUDGET_EXCEEDED", "back off, then retry"),
        P::ProjectionHudUnavailable => ("UNAVAILABLE", "the HUD is unavailable; retry later"),
        P::ProjectionInternalError => ("INTERNAL", "retry once"),
    }
}

/// The owner token the portal holds for this holding is gone.
pub const fn is_stale_token(error_code: ProjectionErrorCode) -> bool {
    use ProjectionErrorCode as P;
    matches!(
        error_code,
        P::ProjectionNotFound | P::ProjectionUnauthorized | P::ProjectionTokenExpired
    )
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
    fn projection_rejections_map_to_shared_codes() {
        for code in ALL_PROJECTION_CODES {
            let (shared, hint) = map_projection(code);
            assert!(ERROR_CODES.contains(&shared), "{shared}");
            assert!(!hint.is_empty());
            assert!(!shared.starts_with("PROJECTION_"));
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
