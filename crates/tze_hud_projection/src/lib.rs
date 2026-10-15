//! Shared in-process portal state and resident rendering support.
//!
//! PortalHub owns authenticated-agent portal identity, transcript retention,
//! viewer replies, liveness and render pacing. The optional resident adapter
//! materializes its presentation without an external operation authority.

pub mod hub;
pub use hub::*;

#[cfg(feature = "resident-grpc")]
pub mod resident_grpc;

#[cfg(all(test, feature = "resident-grpc"))]
mod render_goldens;
