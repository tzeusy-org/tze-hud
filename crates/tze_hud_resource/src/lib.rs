//! # tze_hud_resource
//!
//! Content-addressed resource store for tze_hud: BLAKE3 content addressing,
//! upload validation, the runtime widget asset store, and the resident-memory ledger.
//!
//! Uploads are admitted against hard caps before storage.
//!
//! ## Contents
//!
//! - **BLAKE3 content addressing**: `ResourceId` = 32-byte BLAKE3 digest of raw bytes.
//! - **Deduplication**: `ResourceStore` checks `expected_hash` on upload start; returns
//!   existing resource with `was_deduplicated = true` if found.
//! - **Inline fast path**: resources ≤ 64 KiB upload in a single message.
//! - **Chunked upload**: three-phase flow for resources > 64 KiB.
//! - **Validation**: `ResourceStore` applies capability, size, type, hash,
//!   decode and budget checks before insertion.
//! - **Concurrent upload limits**: max 4 per agent.
//! - **Ephemerality**: uploaded scene resources are kept in memory; the runtime
//!   widget store reindexes its persisted SVG assets at startup.
//!
//! ## Crate structure
//!
//! | Module | Contents |
//! |---|---|
//! | [`types`] | `ResourceId`, `ResourceType`, error codes, size constants |
//! | [`debug`] | Operator/debug hex representation for `ResourceId` |
//! | [`dedup`] | Content-addressed dedup index (`DedupIndex`, `ResourceRecord`) |
//! | [`validation`] | Individual upload and decode checks |
//! | [`upload`] | Upload state machine and `ResourceStore` |

pub mod debug;
pub mod dedup;
pub mod font_bytes_store;
pub mod resident_ledger;
pub mod runtime_widget_store;
pub mod types;
pub mod upload;
pub mod validation;

pub use debug::{resource_id_hex, to_lowercase_hex};
pub use font_bytes_store::FontBytesStore;
pub use resident_ledger::{
    AllocationId, ResidentClass, ResidentLedger, ResidentLedgerLimits, ResidentLedgerSnapshot,
    ResidentReserveError,
};
pub use runtime_widget_store::{
    RuntimeWidgetStore, RuntimeWidgetStoreConfig, RuntimeWidgetStoreError,
};
pub use types::{
    CHUNK_SIZE_LIMIT, DEFAULT_MAX_CONCURRENT_RESOURCES, DEFAULT_MAX_DECODED_TEXTURE_BYTES,
    DEFAULT_MAX_RESOURCE_BYTES, DEFAULT_MAX_TOTAL_TEXTURE_BYTES,
    DEFAULT_UPLOAD_RATE_LIMIT_BYTES_PER_SEC, DecodedMeta, INLINE_SIZE_LIMIT,
    MAX_CONCURRENT_UPLOADS_PER_AGENT, MAX_TEXTURE_DIMENSION_PX, ResourceError, ResourceId,
    ResourceStoreConfig, ResourceStored, ResourceType, SVG_DEFAULT_DIMENSION_PX,
    SVG_MAX_DIMENSION_PX,
};
pub use upload::{ResourceStore, UploadId, UploadStartRequest};
pub use validation::{AgentBudget, CAPABILITY_UPLOAD_RESOURCE, check_capability, check_hash};
