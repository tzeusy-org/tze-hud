//! Timing types for tze_hud.
//!
//! - [`domains`] — clock-domain newtypes for live mutation timestamps:
//!   [`WallUs`] and [`MonoUs`].

pub mod domains;

pub use domains::{MonoUs, WallUs};
