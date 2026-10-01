//! Latency budgets for timing assertions in tests.
//!
//! Budgets are reference-hardware targets. [`test_budget`] widens them by a
//! fixed slack factor so debug builds on shared CI runners don't flake; set
//! `TZE_HUD_TEST_BUDGET_SLACK` (e.g. `1` on a reference host) to tighten or
//! loosen it.

use std::sync::OnceLock;

/// Reference-hardware latency budgets, in microseconds.
pub mod budgets {
    /// Local input acknowledgement (no agent round trip).
    pub const INPUT_ACK_BUDGET_US: u64 = 4_000;
    /// Hit-test against the scene graph.
    pub const HIT_TEST_BUDGET_US: u64 = 100;
    /// Transaction validation per mutation batch.
    pub const TRANSACTION_VALIDATION_BUDGET_US: u64 = 200;
    /// Scene diff computation.
    pub const SCENE_DIFF_BUDGET_US: u64 = 500;
    /// Event dispatch to an agent (hit-test, session lookup, serialization, enqueue).
    pub const EVENT_DISPATCH_BUDGET_US: u64 = 2_000;
}

/// Slack applied when `TZE_HUD_TEST_BUDGET_SLACK` is unset or invalid.
pub const DEFAULT_TEST_BUDGET_SLACK: f64 = 20.0;

/// The slack factor applied by [`test_budget`].
pub fn test_budget_slack() -> f64 {
    static SLACK: OnceLock<f64> = OnceLock::new();
    *SLACK.get_or_init(|| {
        std::env::var("TZE_HUD_TEST_BUDGET_SLACK")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|f| f.is_finite() && *f > 0.0)
            .unwrap_or(DEFAULT_TEST_BUDGET_SLACK)
    })
}

/// A reference budget widened by the test slack factor (at least 1µs).
pub fn test_budget(base_us: u64) -> u64 {
    ((base_us as f64 * test_budget_slack()) as u64).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_is_at_least_the_reference_budget_by_default() {
        if std::env::var_os("TZE_HUD_TEST_BUDGET_SLACK").is_none() {
            assert_eq!(
                test_budget(budgets::HIT_TEST_BUDGET_US),
                budgets::HIT_TEST_BUDGET_US * DEFAULT_TEST_BUDGET_SLACK as u64
            );
        }
        assert!(test_budget(0) >= 1);
    }
}
