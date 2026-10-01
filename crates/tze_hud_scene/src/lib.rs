//! # tze_hud_scene
//!
//! Pure scene graph data model for tze_hud. No GPU dependency.
//! Satisfies DR-V1: Scene model separable from renderer.
//!
//! The scene graph is a tree: Scene → Tab[] → Tile[] → Node[].
//! All types are constructable, mutable, queryable, serializable,
//! and assertable without any GPU context.

pub mod calibration;
pub mod clock;
pub mod diff;
pub mod element_store;
pub mod graph;
pub mod invariants;
pub mod mutation;
pub mod render_wake;
pub mod svg_tokens;
pub mod test_scenes;
pub mod timing;
pub mod types;
pub mod validation;

// ── v1 subsystem trait contracts ─────────────────────────────────────────────
pub mod config;
pub mod events;
pub mod lease;

pub use calibration::{
    CalibrationResult, current_calibration, current_calibration_with_gpu, gpu_scaled_budget,
    scaled_budget, set_gpu_factors, test_budget, texture_upload_scaled_budget,
};
pub use clock::{Clock, SimulatedClock, SystemClock, TestClock};
pub use diff::{DiffEntry, SceneDiff};
pub use element_store::{
    ElementStore, ElementStoreEntry, ElementType, ZERO_GEOMETRY_POLICY,
    fallback_geometry_for_element,
};
// TOML serialization and file persistence for `ElementStore` live in
// `tze_hud_runtime::element_store` to keep this crate I/O-free.
pub use graph::{
    MAX_MARKDOWN_BYTES,
    MAX_NODES_PER_TILE,
    MAX_TAB_NAME_BYTES,
    // RFC 0001 §2.1 scene-level capacity constants
    MAX_TABS,
    MAX_TILES_PER_TAB,
    RuntimeOverlayState,
    SceneGraph,
    SyncGroupCommitDecision,
    // RFC 0001 §2.3 zone band reservation
    ZONE_TILE_Z_MIN,
    // Node data validation
    validate_text_markdown_node_data,
};
pub use mutation::{
    BatchTimingHints, MAX_BATCH_SIZE, MutationBatch, MutationResult, SceneMutation,
};
pub use svg_tokens::{is_valid_token_key, resolve_token_placeholders};
pub use test_scenes::{
    ClockMs, InvariantViolation, SceneGraphTestExt, SceneSpec, TestSceneRegistry,
    assert_layer0_invariants,
};
pub use timing::{
    ClockDriftEstimator,
    ClockOffset,
    ClockSyncRequest,
    ClockSyncResponse,
    // Sync group coordination (rig-cruk)
    CommitDecision,
    DEFAULT_SYNC_DRIFT_BUDGET_US,
    // TimingHints and supporting types
    DeliveryPolicy,
    DurationUs,
    // Expiration heap
    ExpirationEntry,
    ExpirationHeap,
    FrameSyncDriftRecord,
    IntakeContext,
    MessageClass,
    MonoUs,
    ORPHAN_GRACE_PERIOD_US,
    OrphanReason,
    // Pending queue
    PendingEntry,
    PendingQueue,
    Schedule,
    SessionClockSync,
    SyncDriftHighAlert,
    SyncGroupArrival,
    SyncGroupEvent,
    SyncGroupOrphanState,
    TileArrival,
    // Staleness
    TileStaleness,
    TimestampValidationInput,
    // Timing configuration
    TimingConfig,
    TimingConfigError,
    // Errors and warnings
    TimingError,
    TimingHints,
    TimingWarning,
    VsyncSyncPoint,
    WallUs,
    apply_decision,
    check_sync_group_ownership,
    compute_spread,
    evaluate_commit,
    evaluate_frame_drift,
    // Drift detection / ClockSync
    handle_clock_sync,
    // Scheduling helpers
    is_in_scope_for_frame,
    // Relative scheduling primitives (rig-wu3q)
    resolve_after_us,
    resolve_frames_from_now,
    resolve_next_frame,
    resolve_schedule,
    validate_timing_hints,
};
pub use types::*;
pub use validation::{BatchRejected, BatchValidationError, ValidationError, ValidationErrorCode};

// ── Lease governance public API ───────────────────────────────────────────────
pub use lease::capability::{
    CapabilityRevocationError, ZonePublishError, check_zone_publish, has_publish_zone_capability,
    revoke_capability_from_lease, should_clear_on_revoke,
};
pub use lease::degradation::DegradationLevel;
pub use lease::priority::{
    PRIORITY_DEFAULT, PRIORITY_HIGH, PRIORITY_SYSTEM, TileSortKey, clamp_requested_priority,
};
