//! Provider-neutral cooperative HUD projection operation contract.
//!
//! This crate owns the low-token operation schema for the external projection
//! authority described by `openspec/changes/cooperative-hud-projection/`.
//! It deliberately models projection-daemon operations, not runtime v1 MCP
//! tools. If the contract is exposed through MCP, that MCP server belongs to
//! the projection daemon and talks outward to the HUD over the resident control
//! plane.

#[cfg(feature = "resident-grpc")]
pub mod resident_grpc;

/// Portal cadence coalescing with cross-portal fairness.
///
/// Lives here (rather than in `tze_hud_runtime`) so `ProjectionAuthority` can
/// hold a `PortalCadenceCoalescer` without a circular crate dependency.
/// `tze_hud_runtime` re-exports all public items from this module.
pub mod portal_cadence;

mod contract;
pub use self::contract::*;

mod authority;
pub use self::authority::*;

/// Default maximum bytes accepted by one `publish_output` request.
pub const DEFAULT_MAX_OUTPUT_BYTES_PER_CALL: usize = 16_384;
/// Default maximum bytes accepted by `publish_status.status_text`.
pub const DEFAULT_MAX_STATUS_TEXT_BYTES: usize = 512;
/// Default retained transcript byte budget for a projection.
pub const DEFAULT_MAX_RETAINED_TRANSCRIPT_BYTES: usize = 262_144;
/// Default visible transcript byte budget for portal materialization.
pub const DEFAULT_MAX_VISIBLE_TRANSCRIPT_BYTES: usize = 16_384;
/// Default maximum number of pending HUD input items.
pub const DEFAULT_MAX_PENDING_INPUT_ITEMS: usize = 32;
/// Default maximum bytes in one HUD input item.
pub const DEFAULT_MAX_PENDING_INPUT_BYTES_PER_ITEM: usize = 4_096;
/// Default maximum aggregate pending HUD input bytes.
pub const DEFAULT_MAX_PENDING_INPUT_TOTAL_BYTES: usize = 32_768;
/// Default maximum pending items returned by one poll.
pub const DEFAULT_MAX_POLL_ITEMS: usize = 32;
/// Default maximum bytes returned by one pending-input poll.
pub const DEFAULT_MAX_POLL_RESPONSE_BYTES: usize = 16_384;
/// Default maximum projection summaries returned by one caller-scoped list.
pub const DEFAULT_MAX_LIST_ITEMS: usize = 8;
/// Default maximum HUD portal updates per second.
pub const DEFAULT_MAX_PORTAL_UPDATES_PER_SECOND: u32 = 10;
/// Default maximum retained publish-output logical-unit IDs per projection.
pub const DEFAULT_MAX_SEEN_LOGICAL_UNITS: usize = 4_096;
/// Owner tokens are 256-bit random values encoded as lowercase hex.
pub const OWNER_TOKEN_ENTROPY_BITS: usize = 256;
/// Default owner-token lifetime in wall-clock microseconds.
pub const DEFAULT_OWNER_TOKEN_TTL_WALL_US: u64 = 24 * 60 * 60 * 1_000_000;
/// Default authenticated-operation gap before an attached projection is stale.
pub const DEFAULT_AGENT_LIVENESS_DEGRADED_AFTER_WALL_US: u64 = 30 * 1_000_000;
/// Smallest accepted agent-liveness degradation threshold (one second).
pub const MIN_AGENT_LIVENESS_DEGRADED_AFTER_WALL_US: u64 = 1_000_000;
/// Largest accepted agent-liveness degradation threshold (one hour).
pub const MAX_AGENT_LIVENESS_DEGRADED_AFTER_WALL_US: u64 = 60 * 60 * 1_000_000;
/// One wall-clock second in microseconds, used for portal update-rate windows.
pub const PORTAL_UPDATE_RATE_WINDOW_WALL_US: u64 = 1_000_000;

pub(crate) const MAX_PROJECTION_ID_BYTES: usize = 128;
pub(crate) const MAX_REQUEST_ID_BYTES: usize = 128;
pub(crate) const MAX_CALLER_IDENTITY_BYTES: usize = 256;
pub(crate) const MAX_DISPLAY_NAME_BYTES: usize = 128;
pub(crate) const MAX_HINT_BYTES: usize = 256;
pub(crate) const MAX_STATUS_SUMMARY_BYTES: usize = 512;
pub(crate) const MAX_REASON_BYTES: usize = 512;
pub(crate) const MAX_ACK_MESSAGE_BYTES: usize = 512;
pub(crate) const MAX_PORTAL_ID_BYTES: usize = 192;
pub(crate) const DEFAULT_PORTAL_INPUT_TTL_WALL_US: u64 = 10 * 60 * 1_000_000;

#[cfg(test)]
mod tests;
