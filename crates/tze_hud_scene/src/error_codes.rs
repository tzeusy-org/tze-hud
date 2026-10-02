//! The closed set of request error codes, shared by the MCP and gRPC planes
//! (invariant 8). `docs/api.md` documents each one; tests in both protocol
//! crates check that every code they emit is listed here.

use crate::validation::ValidationError;

/// Codes a failed request (MCP tool call or gRPC `RequestResult`) may carry.
pub const ERROR_CODES: &[&str] = &[
    "INVALID_ARGUMENT",
    "NOT_ALLOWED",
    "NOT_HELD",
    "ZONE_NOT_FOUND",
    "WIDGET_NOT_FOUND",
    "WIDGET_PARAMETER_INVALID",
    "CONTENT_REJECTED",
    "LEASE_NOT_ACTIVE",
    "BUDGET_EXCEEDED",
    "SAFE_MODE_ACTIVE",
    "TIMESTAMP_TOO_OLD",
    "TIMESTAMP_TOO_FUTURE",
    "TIMESTAMP_EXPIRY_BEFORE_PRESENT",
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

/// The request error code for a scene validation failure. Callers pass the
/// error's own text as the hint, so the specific reason reaches the agent.
pub fn validation_error_code(e: &ValidationError) -> &'static str {
    use ValidationError as V;
    match e {
        V::ZoneNotFound { .. } => "ZONE_NOT_FOUND",
        V::WidgetNotFound { .. } => "WIDGET_NOT_FOUND",
        V::WidgetUnknownParameter { .. }
        | V::WidgetParameterTypeMismatch { .. }
        | V::WidgetParameterInvalidValue { .. } => "WIDGET_PARAMETER_INVALID",
        V::ZoneMediaTypeMismatch { .. }
        | V::ZoneMaxPublishersReached { .. }
        | V::ZoneMaxKeysReached { .. }
        | V::ZonePublishTokenInvalid { .. }
        | V::WidgetMaxPublishersReached { .. } => "CONTENT_REJECTED",
        V::ZonePublishSafeModeActive { .. } => "SAFE_MODE_ACTIVE",
        V::LeaseNotFound { .. }
        | V::LeaseExpired { .. }
        | V::ZonePublishLeaseNotFound { .. }
        | V::ZonePublishLeaseNotActive { .. }
        | V::ZonePublishLeaseOrphaned { .. } => "LEASE_NOT_ACTIVE",
        V::BudgetExceeded { .. } | V::NodeCountExceeded { .. } | V::BatchSizeExceeded { .. } => {
            "BUDGET_EXCEEDED"
        }
        V::TileNotFound { .. } | V::NamespaceMismatch { .. } => "NOT_HELD",
        _ => "INVALID_ARGUMENT",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_codes_are_in_the_closed_set() {
        let samples = [
            ValidationError::ZoneNotFound { name: "z".into() },
            ValidationError::BudgetExceeded {
                resource: "tiles".into(),
            },
            ValidationError::TileNotFound {
                id: crate::SceneId::new(),
            },
            ValidationError::InvalidField {
                field: "f".into(),
                reason: "r".into(),
            },
        ];
        for e in &samples {
            assert!(ERROR_CODES.contains(&validation_error_code(e)));
        }
    }

    #[test]
    fn codes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in ERROR_CODES {
            assert!(seen.insert(c), "{c} listed twice");
        }
    }
}
