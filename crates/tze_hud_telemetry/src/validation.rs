//! # Performance Validation Harness (Layer 3)
//!
//! Hardware-normalized budget assertions for Layer 3 of the five-validation-layer
//! architecture (see `heart-and-soul/validation.md`).
//!
//! ## Overview
//!
//! Raw timing numbers are meaningless across machines.  All performance budgets
//! are expressed in normalized units using a [`HardwareFactors`] calibration
//! vector.  Each dimension captures a different bottleneck shape:
//!
//! - `cpu`:    scene-graph mutation throughput (from `tze_hud_scene::calibration`)
//! - `gpu`:    fill/composition throughput (headless render loop)
//! - `upload`: texture-upload throughput (create+destroy texture-backed tiles)
//!
//! A CI runner on llvmpipe might have `{cpu: 0.8, gpu: 0.12, upload: 0.15}` —
//! scene-graph tests run near native speed while GPU-bound tests need wide
//! normalization.
//!
//! ## Uncalibrated mode
//!
//! When full calibration is not available (e.g., no GPU in the test environment),
//! [`BudgetAssertion::check`] returns a structured
//! [`AssertionOutcome::Uncalibrated`] warning instead of pass/fail.  This ensures
//! CI never incorrectly fails due to missing calibration data.
//!
//! ```json
//! {"status": "uncalibrated", "reason": "gpu factor not available", "raw_value": 12345}
//! ```
//!
//! ## Normalized budget assertions
//!
//! The following p99 budgets are enforced when calibrated:
//!
//! | Metric                    | Budget        | Dimension |
//! |---------------------------|---------------|-----------|
//! | frame_time                | 16.6ms        | gpu       |
//! | input_to_local_ack        | 4ms           | cpu       |
//! | input_to_scene_commit     | 50ms          | cpu       |
//! | input_to_next_present     | 33ms          | gpu       |
//! | lease_violations          | 0             | n/a       |
//! | budget_overruns           | 0             | n/a       |
//! | sync_drift                | <500µs        | cpu       |

use serde::{Deserialize, Serialize};

use crate::record::{LatencyBucket, SessionSummary};

// ─── Hardware factors ─────────────────────────────────────────────────────────

/// Hardware calibration vector with three independent dimensions.
///
/// Each factor is the ratio of reference throughput to observed throughput.
/// - Factor = 1.0 → this machine matches the reference hardware.
/// - Factor = 2.0 → this machine is 2× slower than reference.
/// - Factor = 0.5 → this machine is 2× faster than reference.
///
/// `None` means calibration for that dimension was not performed or failed
/// (triggers uncalibrated mode for assertions that use it).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HardwareFactors {
    /// CPU scene-graph throughput factor (from `tze_hud_scene::calibration`).
    pub cpu: Option<f64>,
    /// GPU fill/composition throughput factor (from headless render loop).
    pub gpu: Option<f64>,
    /// Texture upload throughput factor (from upload workload).
    pub upload: Option<f64>,
}

impl HardwareFactors {
    /// Create a fully uncalibrated set (all None).
    pub fn uncalibrated() -> Self {
        Self {
            cpu: None,
            gpu: None,
            upload: None,
        }
    }

    /// Create a calibrated set with known factors.
    pub fn new(cpu: f64, gpu: f64, upload: f64) -> Self {
        Self {
            cpu: Some(cpu),
            gpu: Some(gpu),
            upload: Some(upload),
        }
    }

    /// Create a CPU-only calibrated set (GPU/upload uncalibrated).
    pub fn cpu_only(cpu: f64) -> Self {
        Self {
            cpu: Some(cpu),
            gpu: None,
            upload: None,
        }
    }

    /// Returns true if all three dimensions are calibrated.
    pub fn is_fully_calibrated(&self) -> bool {
        self.cpu.is_some() && self.gpu.is_some() && self.upload.is_some()
    }
}

impl Default for HardwareFactors {
    fn default() -> Self {
        Self::uncalibrated()
    }
}

// ─── Calibration dimension ───────────────────────────────────────────────────

/// Which hardware dimension normalizes a given budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationDimension {
    Cpu,
    Gpu,
    Upload,
    /// Budget does not use hardware normalization (e.g., zero-violation counters).
    None,
}

impl CalibrationDimension {
    /// Extract the factor for this dimension from a [`HardwareFactors`].
    pub fn factor_from(&self, factors: &HardwareFactors) -> Option<f64> {
        match self {
            Self::Cpu => factors.cpu,
            Self::Gpu => factors.gpu,
            Self::Upload => factors.upload,
            Self::None => Some(1.0),
        }
    }
}

// ─── Assertion outcome ────────────────────────────────────────────────────────

/// Result of a single budget assertion.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AssertionOutcome {
    /// Assertion passed: p99 value is within the calibrated budget.
    Pass {
        /// The name of the metric being checked.
        metric: String,
        /// Observed p99 value (microseconds or count depending on metric).
        observed: u64,
        /// Calibrated budget that was enforced.
        budget: u64,
        /// Hardware factor applied.
        factor: f64,
    },
    /// Assertion failed: p99 value exceeds the calibrated budget.
    Fail {
        metric: String,
        observed: u64,
        budget: u64,
        factor: f64,
        /// Overage amount (observed - budget).
        overage: u64,
        /// Overage percentage.
        overage_pct: f64,
    },
    /// Calibration not available for this metric's dimension.
    ///
    /// The assertion cannot pass or fail — raw value is reported for
    /// informational purposes only.
    Uncalibrated {
        metric: String,
        reason: String,
        raw_value: u64,
    },
    /// No samples recorded for this metric — cannot assert.
    NoSamples { metric: String },
}

impl AssertionOutcome {
    /// Returns true if the outcome is a definitive pass.
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass { .. })
    }

    /// Returns true if the outcome is a definitive fail.
    pub fn is_fail(&self) -> bool {
        matches!(self, Self::Fail { .. })
    }

    /// Returns true if uncalibrated (neither pass nor fail).
    pub fn is_uncalibrated(&self) -> bool {
        matches!(self, Self::Uncalibrated { .. })
    }
}

// ─── Budget assertion ─────────────────────────────────────────────────────────

/// A single normalized budget assertion for a latency bucket.
///
/// Scaling: `effective_budget = base_budget_us * factor`
/// where `factor` comes from the hardware calibration dimension.
pub struct BudgetAssertion {
    /// Human-readable name for this metric.
    pub metric: String,
    /// Base budget on reference hardware (microseconds).
    pub base_budget_us: u64,
    /// Which dimension normalizes this budget.
    pub dimension: CalibrationDimension,
}

impl BudgetAssertion {
    /// Create a new budget assertion.
    pub fn new(
        metric: impl Into<String>,
        base_budget_us: u64,
        dimension: CalibrationDimension,
    ) -> Self {
        Self {
            metric: metric.into(),
            base_budget_us,
            dimension,
        }
    }

    /// Check a `LatencyBucket` against this budget with the given hardware factors.
    ///
    /// - If the bucket has no samples → `NoSamples`.
    /// - If the required dimension is not calibrated → `Uncalibrated`.
    /// - If p99 ≤ effective_budget → `Pass`.
    /// - If p99 > effective_budget → `Fail`.
    pub fn check(&self, bucket: &LatencyBucket, factors: &HardwareFactors) -> AssertionOutcome {
        // Check for samples first; a missing dimension on an empty bucket is
        // meaningless — report the data absence, not the calibration state.
        let p99 = match bucket.p99() {
            Some(v) => v,
            None => {
                return AssertionOutcome::NoSamples {
                    metric: self.metric.clone(),
                };
            }
        };

        let factor = match self.dimension.factor_from(factors) {
            Some(f) => f,
            None => {
                return AssertionOutcome::Uncalibrated {
                    metric: self.metric.clone(),
                    reason: format!("{:?} factor not available", self.dimension),
                    raw_value: p99,
                };
            }
        };

        // Use ceil to avoid truncation making the enforced budget stricter than intended.
        let effective_budget = (self.base_budget_us as f64 * factor).ceil().max(1.0) as u64;

        if p99 <= effective_budget {
            AssertionOutcome::Pass {
                metric: self.metric.clone(),
                observed: p99,
                budget: effective_budget,
                factor,
            }
        } else {
            let overage = p99 - effective_budget;
            let overage_pct = (p99 as f64 / effective_budget as f64 - 1.0) * 100.0;
            AssertionOutcome::Fail {
                metric: self.metric.clone(),
                observed: p99,
                budget: effective_budget,
                factor,
                overage,
                overage_pct,
            }
        }
    }

    /// Check a raw count value (e.g., lease_violations, budget_overruns) must be zero.
    ///
    /// For zero-violation counters the calibration dimension is always `None`.
    pub fn check_zero(metric: impl Into<String>, value: u64) -> AssertionOutcome {
        let metric = metric.into();
        if value == 0 {
            AssertionOutcome::Pass {
                metric,
                observed: 0,
                budget: 0,
                factor: 1.0,
            }
        } else {
            AssertionOutcome::Fail {
                metric,
                observed: value,
                budget: 0,
                factor: 1.0,
                overage: value,
                overage_pct: f64::INFINITY,
            }
        }
    }
}

/// Full set of Layer-3 assertion outcomes for a session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidationReport {
    /// Hardware factors used for normalization.
    pub hardware_factors: HardwareFactors,
    /// Individual assertion outcomes.
    pub assertions: Vec<AssertionOutcome>,
    /// Number of definitive passes.
    pub pass_count: usize,
    /// Number of definitive failures.
    pub fail_count: usize,
    /// Number of uncalibrated / no-samples outcomes.
    pub uncalibrated_count: usize,
    /// Overall verdict string. One of:
    /// - `"pass"`: all assertions produced a definitive pass, no failures.
    /// - `"fail"`: at least one assertion definitively failed.
    /// - `"uncalibrated"`: no assertions produced a definitive pass or fail
    ///   (all were uncalibrated or had no samples).
    /// - `"partial"`: some assertions passed while others were uncalibrated
    ///   or had no samples.
    pub verdict: String,
}

impl ValidationReport {
    /// Run all Layer-3 assertions against a `SessionSummary`.
    ///
    /// Assertions checked:
    /// - `frame_time` p99 < 16.6ms (GPU dimension)
    /// - `input_to_local_ack` p99 < 4ms (CPU dimension)
    /// - `input_to_scene_commit` p99 < 50ms (CPU dimension)
    /// - `input_to_next_present` p99 < 33ms (GPU dimension)
    /// - `lease_violations` == 0
    /// - `budget_overruns` == 0
    /// - `sync_drift_violations` == 0
    pub fn run(summary: &SessionSummary, factors: &HardwareFactors) -> Self {
        let assertions_to_run = [
            BudgetAssertion::new("frame_time_p99", 16_600, CalibrationDimension::Gpu),
            BudgetAssertion::new("input_to_local_ack_p99", 4_000, CalibrationDimension::Cpu),
            BudgetAssertion::new(
                "input_to_scene_commit_p99",
                50_000,
                CalibrationDimension::Cpu,
            ),
            BudgetAssertion::new(
                "input_to_next_present_p99",
                33_000,
                CalibrationDimension::Gpu,
            ),
        ];

        let mut outcomes: Vec<AssertionOutcome> = assertions_to_run
            .iter()
            .map(|a| {
                let bucket = match a.metric.as_str() {
                    "frame_time_p99" => &summary.frame_time,
                    "input_to_local_ack_p99" => &summary.input_to_local_ack,
                    "input_to_scene_commit_p99" => &summary.input_to_scene_commit,
                    "input_to_next_present_p99" => &summary.input_to_next_present,
                    _ => unreachable!(),
                };
                a.check(bucket, factors)
            })
            .collect();

        // Zero-violation counters
        outcomes.push(BudgetAssertion::check_zero(
            "lease_violations",
            summary.lease_violations,
        ));
        outcomes.push(BudgetAssertion::check_zero(
            "budget_overruns",
            summary.budget_overruns,
        ));
        outcomes.push(BudgetAssertion::check_zero(
            "sync_drift_violations",
            summary.sync_drift_violations,
        ));

        let pass_count = outcomes.iter().filter(|o| o.is_pass()).count();
        let fail_count = outcomes.iter().filter(|o| o.is_fail()).count();
        let uncalibrated_count = outcomes
            .iter()
            .filter(|o| o.is_uncalibrated() || matches!(o, AssertionOutcome::NoSamples { .. }))
            .count();

        let verdict = if fail_count > 0 {
            "fail".to_string()
        } else if uncalibrated_count > 0 && pass_count == 0 {
            "uncalibrated".to_string()
        } else if uncalibrated_count > 0 {
            // Some passed, some uncalibrated — partial
            "partial".to_string()
        } else {
            "pass".to_string()
        };

        Self {
            hardware_factors: factors.clone(),
            assertions: outcomes,
            pass_count,
            fail_count,
            uncalibrated_count,
            verdict,
        }
    }

    /// Serialize to pretty-printed JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bucket(name: &str, values: &[u64]) -> LatencyBucket {
        let mut b = LatencyBucket::new(name);
        for &v in values {
            b.record(v);
        }
        b
    }

    // ── HardwareFactors ───────────────────────────────────────────────────────

    #[test]
    fn hardware_factors_uncalibrated_all_none() {
        let f = HardwareFactors::uncalibrated();
        assert!(!f.is_fully_calibrated());
        assert!(f.cpu.is_none());
        assert!(f.gpu.is_none());
        assert!(f.upload.is_none());
    }

    #[test]
    fn hardware_factors_fully_calibrated() {
        let f = HardwareFactors::new(1.0, 1.5, 2.0);
        assert!(f.is_fully_calibrated());
    }

    #[test]
    fn hardware_factors_cpu_only() {
        let f = HardwareFactors::cpu_only(1.2);
        assert!(f.cpu.is_some());
        assert!(f.gpu.is_none());
        assert!(f.upload.is_none());
        assert!(!f.is_fully_calibrated());
    }

    // ── CalibrationDimension ─────────────────────────────────────────────────

    #[test]
    fn dimension_none_always_returns_one() {
        let f = HardwareFactors::uncalibrated();
        assert_eq!(CalibrationDimension::None.factor_from(&f), Some(1.0));
    }

    #[test]
    fn dimension_cpu_returns_cpu_factor() {
        let f = HardwareFactors::cpu_only(2.5);
        assert_eq!(CalibrationDimension::Cpu.factor_from(&f), Some(2.5));
        assert_eq!(CalibrationDimension::Gpu.factor_from(&f), None);
    }

    // ── BudgetAssertion ───────────────────────────────────────────────────────

    #[test]
    fn assertion_pass_within_budget() {
        let assertion = BudgetAssertion::new("frame_time_p99", 16_600, CalibrationDimension::Gpu);
        let factors = HardwareFactors::new(1.0, 1.0, 1.0);
        // 100 samples of 12ms — well within 16.6ms budget
        let bucket = make_bucket("frame_time", &vec![12_000u64; 100]);
        let outcome = assertion.check(&bucket, &factors);
        assert!(outcome.is_pass(), "expected pass, got {outcome:?}");
    }

    #[test]
    fn assertion_fail_exceeds_budget() {
        let assertion = BudgetAssertion::new("frame_time_p99", 16_600, CalibrationDimension::Gpu);
        let factors = HardwareFactors::new(1.0, 1.0, 1.0);
        // 100 samples of 20ms — over the 16.6ms budget
        let bucket = make_bucket("frame_time", &vec![20_000u64; 100]);
        let outcome = assertion.check(&bucket, &factors);
        assert!(outcome.is_fail(), "expected fail, got {outcome:?}");
        if let AssertionOutcome::Fail {
            observed, budget, ..
        } = &outcome
        {
            assert_eq!(*observed, 20_000);
            assert_eq!(*budget, 16_600);
        }
    }

    #[test]
    fn assertion_uncalibrated_when_dimension_missing() {
        let assertion = BudgetAssertion::new("frame_time_p99", 16_600, CalibrationDimension::Gpu);
        // GPU factor missing → uncalibrated
        let factors = HardwareFactors::cpu_only(1.0);
        let bucket = make_bucket("frame_time", &vec![20_000u64; 100]);
        let outcome = assertion.check(&bucket, &factors);
        assert!(
            outcome.is_uncalibrated(),
            "expected uncalibrated, got {outcome:?}"
        );
        if let AssertionOutcome::Uncalibrated {
            raw_value, reason, ..
        } = &outcome
        {
            assert_eq!(*raw_value, 20_000);
            assert!(
                reason.contains("Gpu"),
                "reason should mention Gpu: {reason}"
            );
        }
    }

    #[test]
    fn assertion_no_samples_when_bucket_empty() {
        let assertion = BudgetAssertion::new("frame_time_p99", 16_600, CalibrationDimension::Gpu);
        let factors = HardwareFactors::new(1.0, 1.0, 1.0);
        let bucket = make_bucket("frame_time", &[]);
        let outcome = assertion.check(&bucket, &factors);
        assert!(matches!(outcome, AssertionOutcome::NoSamples { .. }));
    }

    #[test]
    fn assertion_scales_budget_with_factor() {
        // On a 2x slower machine, the effective budget doubles
        let assertion = BudgetAssertion::new("frame_time_p99", 16_600, CalibrationDimension::Gpu);
        let factors = HardwareFactors::new(1.0, 2.0, 1.0);
        // 20ms should fail on reference hardware (factor=1.0)
        // but pass on a 2x slower machine (budget=33.2ms)
        let bucket = make_bucket("frame_time", &vec![20_000u64; 100]);
        let outcome = assertion.check(&bucket, &factors);
        assert!(
            outcome.is_pass(),
            "on 2x slower machine 20ms should be within 33.2ms budget, got {outcome:?}"
        );
    }

    #[test]
    fn check_zero_passes_for_zero() {
        let outcome = BudgetAssertion::check_zero("lease_violations", 0);
        assert!(outcome.is_pass());
    }

    #[test]
    fn check_zero_fails_for_nonzero() {
        let outcome = BudgetAssertion::check_zero("lease_violations", 3);
        assert!(outcome.is_fail());
        if let AssertionOutcome::Fail { observed, .. } = &outcome {
            assert_eq!(*observed, 3);
        }
    }

    // ── ValidationReport ─────────────────────────────────────────────────────

    #[test]
    fn report_pass_when_all_within_budget() {
        let mut summary = SessionSummary::new();
        // 100 samples of 12ms frame time
        for _ in 0..100 {
            summary.frame_time.record(12_000);
            summary.input_to_local_ack.record(2_000);
            summary.input_to_scene_commit.record(20_000);
            summary.input_to_next_present.record(25_000);
        }

        let factors = HardwareFactors::new(1.0, 1.0, 1.0);
        let report = ValidationReport::run(&summary, &factors);
        assert_eq!(report.verdict, "pass", "report: {:?}", report.assertions);
        assert_eq!(report.fail_count, 0);
    }

    #[test]
    fn report_fail_when_frame_time_over_budget() {
        let mut summary = SessionSummary::new();
        for _ in 0..100 {
            summary.frame_time.record(25_000); // 25ms > 16.6ms
        }
        let factors = HardwareFactors::new(1.0, 1.0, 1.0);
        let report = ValidationReport::run(&summary, &factors);
        assert_eq!(report.verdict, "fail");
        assert!(report.fail_count > 0);
    }

    #[test]
    fn report_fail_for_lease_violations() {
        let mut summary = SessionSummary::new();
        for _ in 0..100 {
            summary.frame_time.record(10_000);
            summary.input_to_local_ack.record(1_000);
            summary.input_to_scene_commit.record(10_000);
            summary.input_to_next_present.record(15_000);
        }
        summary.lease_violations = 2;
        let factors = HardwareFactors::new(1.0, 1.0, 1.0);
        let report = ValidationReport::run(&summary, &factors);
        assert_eq!(report.verdict, "fail");
    }

    #[test]
    fn report_uncalibrated_when_no_gpu_factor() {
        let mut summary = SessionSummary::new();
        for _ in 0..100 {
            summary.input_to_local_ack.record(2_000);
            summary.input_to_scene_commit.record(20_000);
        }
        // Only CPU calibrated; GPU metrics will be uncalibrated
        let factors = HardwareFactors::cpu_only(1.0);
        let report = ValidationReport::run(&summary, &factors);
        // Should not be "fail" — uncalibrated is not a failure
        assert_ne!(report.verdict, "fail", "report: {:?}", report.assertions);
        assert!(report.uncalibrated_count > 0);
    }

    #[test]
    fn report_serializes_to_json() {
        let summary = SessionSummary::new();
        let factors = HardwareFactors::uncalibrated();
        let report = ValidationReport::run(&summary, &factors);
        let json = report.to_json().unwrap();
        assert!(json.contains("verdict"));
        assert!(json.contains("assertions"));
        assert!(json.contains("hardware_factors"));
    }
}
