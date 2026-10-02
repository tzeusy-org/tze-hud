//! Lease identity type.

use crate::types::SceneId;

/// UUIDv7 lease identifier.  Time-ordered; assigned by the runtime at grant time.
pub type LeaseId = SceneId;
