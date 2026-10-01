//! # tze_hud_telemetry
//!
//! Structured telemetry for tze_hud. Per-frame timing, throughput,
//! and resource metrics emitted as machine-readable JSON.
//! Satisfies DR-V3: Structured telemetry.
//! Satisfies DR-V8: Soak and leak test resource monitoring.

pub mod collector;
pub mod idle_efficiency;
pub mod invalidation_closure;
pub mod publish_load;
pub mod record;
pub mod resource_monitor;
pub mod validation;

pub use collector::{FrameRecorder, TelemetryCollector};
pub use idle_efficiency::{
    ConstrainedProfileIdentity, EfficiencyBudgetResult, EfficiencyGpuCounters,
    EfficiencyPacingIdentity, EfficiencyPacingMode, EfficiencyRendererIdentity,
    EfficiencyRuntimeIdentity, EfficiencyScenarioIdentity, EfficiencyViewport,
    EfficiencyWakeupCounters, EfficiencyWindowMode, QUIESCENT_EFFICIENCY_SCHEMA_VERSION,
    QUIESCENT_INTERVAL_MIN_MS, QUIESCENT_RUNTIME_WAKEUP_CEILING, QUIESCENT_SCENARIO_NAME,
    QUIESCENT_SCENARIO_VERSION, QUIESCENT_SETTLING_MIN_MS, QuiescentEfficiencyArtifact,
    QuiescentEfficiencyValidation, QuiescentMeasurementStatus,
};
pub use invalidation_closure::{
    ActualWorkItem, AmplificationRatio, CHANGE_EFFICIENCY_SCHEMA_VERSION, ChangeEfficiencyArtifact,
    ChangeEfficiencyCategoryReport, ChangeEfficiencyDamageReport,
    ChangeEfficiencyTextureUploadReport, ChangeEfficiencyValidation,
    ChangeEfficiencyValidationStatus, ChangeMeasurementProvenance, ChangeMeasurementStatus,
    ChangeRenderWorkObservation, ClosureWorkItem, DamageCategory, DamageWorkItemId,
    FullSurfaceInvalidation, FullSurfaceInvalidationReason, InvalidationCategory,
    InvalidationClosure, InvalidationDependencyReason, NodeWorkItemId,
    ONE_NODE_FIFTY_TILE_SCENARIO_NAME, ONE_NODE_FIFTY_TILE_SCENARIO_VERSION,
    PartialPresentCapability, PixelRect, RenderPlanWorkItemId,
    TRANSPARENT_OVERLAP_FIFTY_TILE_SCENARIO_NAME, TRANSPARENT_OVERLAP_FIFTY_TILE_SCENARIO_VERSION,
    TextureUploadActualWork, TextureUploadCategory, TextureUploadWorkItemId,
};
pub use publish_load::{
    ByteAccountingMode, PublishLoadArtifact, PublishLoadCalibrationStatus, PublishLoadIdentity,
    PublishLoadMetrics, PublishLoadMode, PublishLoadThresholds, PublishLoadTraceability,
    PublishLoadTransport, PublishLoadVerdict,
};
pub use record::{
    BudgetTier, BudgetViolationEvent, BudgetViolationKind, CalibrationStatus, DegradationDirection,
    DegradationEvent, DegradationRecoverySource, FrameTelemetry, FrameTimeShedEvent, LatencyBucket,
    SessionSummary,
};
pub use resource_monitor::{
    AgentFootprint, GrowthRatios, MutationAccountant, ResourceMonitor, ResourceSnapshot,
    SPEC_GROWTH_TOLERANCE,
};
pub use validation::{
    AssertionOutcome, BudgetAssertion, CalibrationDimension, HardwareFactors, ValidationReport,
};
