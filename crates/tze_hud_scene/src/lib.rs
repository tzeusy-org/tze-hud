//! # tze_hud_scene
//!
//! Pure scene graph data model for tze_hud. No GPU dependency.
//! Satisfies DR-V1: Scene model separable from renderer.
//!
//! The scene graph is a tree: Scene → Tab[] → Tile[] → Node[].
//! All types are constructable, mutable, queryable, serializable,
//! and assertable without any GPU context.

pub mod clock;
pub mod element_store;
pub mod error_codes;
pub mod graph;
pub mod mutation;
pub mod perf_budget;
pub mod portal_reading;
pub mod publication_defaults;
pub mod render_wake;
pub mod svg_tokens;
#[cfg(any(test, feature = "test-support"))]
pub mod test_scenes;
pub mod text_fit;
pub mod timing;
pub mod types;
pub mod validation;
pub mod widget_deadline;

// ── v1 subsystem trait contracts ─────────────────────────────────────────────
pub mod config;
pub mod lease;

pub use clock::{Clock, SystemClock, TestClock};
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
    PendingAction,
    RuntimeOverlayState,
    SceneGraph,
    // RFC 0001 §2.3 zone band reservation
    ZONE_TILE_Z_MIN,
    // Node data validation
    validate_text_markdown_node_data,
};
pub use mutation::{
    BatchTimingHints, MAX_BATCH_SIZE, MutationBatch, MutationResult, SceneMutation,
};
pub use portal_reading::{
    PortalCopyButton, PortalGlyph, PortalLink, PortalReadingDecor, PortalReadingLayout,
    https_link_target,
};
pub use publication_defaults::{
    DEFAULT_ANSWER_WINDOW_MS, MAX_ANSWER_WINDOW_MS, PublicationDefaults,
};
pub use svg_tokens::{is_valid_token_key, resolve_token_placeholders};
#[cfg(any(test, feature = "test-support"))]
pub use test_scenes::{
    ClockMs, InvariantViolation, SceneSpec, TestSceneRegistry, assert_layer0_invariants,
};
pub use text_fit::TextFit;
pub use timing::{MonoUs, WallUs};
pub use types::*;
pub use validation::{BatchRejected, BatchValidationError, ValidationError, ValidationErrorCode};
pub use widget_deadline::WidgetDeadline;

// ── Lease governance public API ───────────────────────────────────────────────
pub use lease::degradation::DegradationLevel;
