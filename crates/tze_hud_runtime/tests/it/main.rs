//! External behavior suites, retained at their original source paths.

// Share one initialization gate across the dev-mode GPU suites.
#[cfg(feature = "dev-mode")]
#[path = "../../src/test_support.rs"]
mod gpu_init;
#[cfg(feature = "dev-mode")]
#[path = "../pixel_helpers.rs"]
mod pixel_helpers;

#[path = "../idle_efficiency_counters.rs"]
mod idle_efficiency_counters;

#[cfg(feature = "dev-mode")]
#[path = "../pixel_readback.rs"]
mod pixel_readback;

#[cfg(feature = "dev-mode")]
#[path = "../present_ack.rs"]
mod present_ack;
