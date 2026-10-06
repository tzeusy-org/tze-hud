//! # tze_hud_telemetry
//!
//! Structured telemetry for tze_hud. Per-frame timing, throughput,
//! and resource metrics emitted as machine-readable JSON.
//! Satisfies DR-V3: Structured telemetry.
//! Satisfies DR-V8: Soak and leak test resource monitoring.

pub mod collector;
pub mod idle_efficiency;
pub mod publish_load;
pub mod record;
pub mod validation;
pub mod work_counts;

pub use collector::TelemetryCollector;
pub use idle_efficiency::{
    ConstrainedProfileIdentity, EfficiencyBudgetResult, EfficiencyGpuCounters,
    EfficiencyPacingIdentity, EfficiencyPacingMode, EfficiencyRendererIdentity,
    EfficiencyRuntimeIdentity, EfficiencyScenarioIdentity, EfficiencyViewport,
    EfficiencyWakeupCounters, EfficiencyWindowMode, QUIESCENT_EFFICIENCY_SCHEMA_VERSION,
    QUIESCENT_INTERVAL_MIN_MS, QUIESCENT_RUNTIME_WAKEUP_CEILING, QUIESCENT_SCENARIO_NAME,
    QUIESCENT_SCENARIO_VERSION, QUIESCENT_SETTLING_MIN_MS, QuiescentEfficiencyArtifact,
    QuiescentEfficiencyValidation, QuiescentMeasurementStatus,
};
pub use publish_load::{
    ByteAccountingMode, PublishLoadArtifact, PublishLoadCalibrationStatus, PublishLoadIdentity,
    PublishLoadMetrics, PublishLoadMode, PublishLoadThresholds, PublishLoadTraceability,
    PublishLoadTransport, PublishLoadVerdict,
};
pub use record::{
    DegradationDirection, DegradationEvent, DegradationRecoverySource, FrameTelemetry,
    LatencyBucket, SessionSummary,
};
pub use validation::{
    AssertionOutcome, BudgetAssertion, CalibrationDimension, HardwareFactors, ValidationReport,
};
pub use work_counts::WorkCounts;
