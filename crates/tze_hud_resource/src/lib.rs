//! # tze_hud_resource
//!
//! Content-addressed resource store for tze_hud — the **resource accounting authority**.
//!
//! ## Authority Boundary
//!
//! This crate owns all decoded-byte budget accounting for uploaded resources
//! (textures, fonts, etc.). It is the single source of truth for:
//!
//! - Whether an agent's decoded resource usage would exceed its per-tile or
//!   per-agent texture budget.
//! - Per-agent reference counts and GC candidacy.
//!
//! The runtime calls into this crate during mutation validation to check and
//! charge resource budgets.
//!
//! **Do not add enforcement-ladder logic here.** The enforcement ladder
//! (Warning → Throttle → Revoke) lives in `tze_hud_runtime::budget::BudgetEnforcer`,
//! which tracks per-agent budget state over time. This crate tracks instantaneous
//! decoded-byte accounting only.
//!
//! ## Contents
//!
//! Implements RFC 0011 upload, deduplication, reference counting, GC, budget
//! accounting, and font cache requirements:
//!
//! - **BLAKE3 content addressing**: `ResourceId` = 32-byte BLAKE3 digest of raw bytes.
//! - **Deduplication**: `DedupIndex` checks `expected_hash` on `ResourceUploadStart`; returns
//!   existing resource with `was_deduplicated = true` if found.
//! - **Inline fast path**: resources ≤ 64 KiB upload in a single message.
//! - **Chunked upload**: three-phase flow for resources > 64 KiB.
//! - **Validation pipeline**: capability, hash integrity, size limits, budget,
//!   type check, decode validation.
//! - **Concurrent upload limits**: max 4 per agent.
//! - **Reference counting**: atomic per-resource refcount; `RefcountLayer` wraps
//!   `DedupIndex` with GC candidacy tracking.
//! - **Garbage collection**: `GcRunner` evicts resources after configurable grace
//!   period (default 60 s) with 5 ms per-cycle budget.
//! - **Per-agent budget accounting**: `BudgetRegistry` charges decoded bytes per
//!   agent node reference (full decoded size, double-counted across agents).
//! - **Cross-agent sharing**: `SharingContext` provides capability-free referencing
//!   (hash knowledge is the capability) and no enumeration API.
//! - **Font cache**: LRU cache with permanent system/bundled holds.
//! - **V1 ephemerality**: all resources stored in memory only; lost on restart.
//!
//! ## Crate structure
//!
//! | Module | Contents |
//! |---|---|
//! | [`types`] | `ResourceId`, `ResourceType`, error codes, size constants |
//! | [`debug`] | Operator/debug hex representation for `ResourceId` |
//! | [`dedup`] | Content-addressed dedup index (`DedupIndex`, `ResourceRecord`) |
//! | [`validation`] | Six-step upload validation pipeline |
//! | [`upload`] | Upload state machine and `ResourceStore` |
//! | [`refcount`] | Scene-graph-level reference counting and GC candidacy |
//! | [`gc`] | GC runner: grace period, cycle timing, frame-render isolation |
//! | [`budget`] | Per-agent decoded-byte budget accounting |
//! | [`sharing`] | Cross-agent sharing semantics: cap-free reference, double-counted budget |
//! | [`font_cache`] | LRU font cache with permanent system/bundled holds |
//! | [`store`] | V1 ephemerality contract (`EphemeralStore`) |

pub mod budget;
pub mod debug;
pub mod dedup;
pub mod font_bytes_store;
pub mod font_cache;
pub mod gc;
pub mod refcount;
pub mod resident_ledger;
pub mod runtime_widget_store;
pub mod sharing;
pub mod store;
pub mod types;
pub mod upload;
pub mod validation;

pub use budget::{AgentResourceUsage, BudgetRegistry, BudgetViolation, TileBudgetChecker};
pub use debug::{resource_id_hex, to_lowercase_hex};
pub use dedup::{DedupIndex, ResourceRecord};
pub use font_bytes_store::FontBytesStore;
pub use font_cache::{CachedFontHandle, FontCache, FontCacheEntry, FontCacheKey, FontOrigin};
pub use gc::{GcClock, GcConfig, GcResult, GcRunner, TestClockMs, WallClock};
pub use refcount::{GcCandidateTable, RefcountError, RefcountLayer};
pub use resident_ledger::{
    AllocationId, ResidentClass, ResidentLedger, ResidentLedgerLimits, ResidentLedgerSnapshot,
    ResidentReserveError,
};
pub use runtime_widget_store::{
    PutOutcome as RuntimeWidgetStorePutOutcome, RuntimeWidgetAssetRecord, RuntimeWidgetStore,
    RuntimeWidgetStoreConfig, RuntimeWidgetStoreError,
};
pub use sharing::{RefResult, SharingContext, check_reference_policy};
pub use store::EphemeralStore;
pub use types::{
    CHUNK_SIZE_LIMIT, DEFAULT_MAX_CONCURRENT_RESOURCES, DEFAULT_MAX_DECODED_TEXTURE_BYTES,
    DEFAULT_MAX_FONT_CACHE_BYTES, DEFAULT_MAX_RESOURCE_BYTES, DEFAULT_MAX_TOTAL_TEXTURE_BYTES,
    DEFAULT_UPLOAD_RATE_LIMIT_BYTES_PER_SEC, DecodedMeta, INLINE_SIZE_LIMIT,
    MAX_CONCURRENT_UPLOADS_PER_AGENT, MAX_TEXTURE_DIMENSION_PX, ResourceError, ResourceId,
    ResourceStoreConfig, ResourceStored, ResourceType, SVG_DEFAULT_DIMENSION_PX,
    SVG_MAX_DIMENSION_PX,
};
pub use upload::{ResourceStore, UploadId, UploadStartRequest};
pub use validation::{AgentBudget, CAPABILITY_UPLOAD_RESOURCE, check_capability, check_hash};
