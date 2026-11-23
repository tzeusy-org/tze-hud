//! External behavior suites, retained at their original source paths.

#[macro_use]
#[path = "../common/mod.rs"]
mod common;

#[path = "../gauge_binding_rendering.rs"]
mod gauge_binding_rendering;

#[path = "../gpu_canary.rs"]
mod gpu_canary;

#[path = "../widget_raster_count.rs"]
mod widget_raster_count;

#[path = "../widget_transition.rs"]
mod widget_transition;

#[path = "../zone_rendering.rs"]
mod zone_rendering;
