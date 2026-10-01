//! # Scene Events
//!
//! Event taxonomy, envelope structure, and naming convention for tze_hud.
//!
//! Implements bead rig-cfln (Bead 1 of Epic 9) against
//! `openspec/changes/v1-mvp-standards/specs/scene-events/spec.md`.
//!
//! ## Module structure
//!
//! - [`taxonomy`] — three-category taxonomy (`EventCategory`) and nine
//!   subscription categories (`SubscriptionCategory`) with prefix routing.
//! - [`naming`]   — naming convention validation, reserved-prefix rejection,
//!   and agent bare-name prefixing.
//! - [`envelope`] — `SceneEvent` envelope, `InterruptionClass`, `EventSource`,
//!   `EventPayload`, and `SceneEventBuilder`.
//!
//! ## Excluded from this bead
//!
//! The following are deferred to later beads in this epic:
//! - Subscription filtering and delivery mechanics (bead #2)
//! - Agent event emission protocol and rate limiting (bead #4)
//! - tab_switch_on_event (bead #4)
//!
//! ## Added in bead #3
//!
//! - [`interruption`] — agent CRITICAL downgrade, zone ceiling enforcement
//!   functions (`apply_agent_class`, `apply_zone_ceiling`, `classify_agent_event`).

pub mod emission;
pub mod envelope;
pub mod interruption;
pub mod naming;
pub mod taxonomy;

// Re-export the most commonly used types at the module root.
pub use emission::{AgentEventRateLimiter, DEFAULT_MAX_EVENTS_PER_SECOND, MAX_PAYLOAD_BYTES};
pub use envelope::{EventPayload, EventSource, InterruptionClass, SceneEvent, SceneEventBuilder};
pub use interruption::{apply_agent_class, apply_zone_ceiling, classify_agent_event};
pub use naming::{NamingError, build_agent_event_type, validate_bare_name, validate_event_type};
pub use taxonomy::{EventCategory, SubscriptionCategory};
