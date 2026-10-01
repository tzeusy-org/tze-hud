//! Timing types for tze_hud.
//!
//! - [`domains`] — clock-domain newtypes: [`WallUs`], [`MonoUs`], [`DurationUs`].
//! - [`hints`] — [`TimingHints`] carried on payloads (`present_at`, `expires_at`,
//!   message class, delivery policy).

pub mod domains;
pub mod hints;

pub use domains::{DurationUs, MonoUs, WallUs};
pub use hints::{DeliveryPolicy, MessageClass, Schedule, TimingHints};
