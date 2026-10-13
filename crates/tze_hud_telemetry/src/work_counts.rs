//! Opt-in observations of one completed headless frame, not a certification.
//!
//! Layout counts text cache-miss shaping calls; raster counts widget raster
//! invocations, including attempts that fail; upload counts widget RGBA
//! `queue.write_texture` submissions. Glyphon atlas, resource and static-image
//! uploads are outside that observation, so zero never means no GPU uploads.
//! Rendering and live raw/frame telemetry remain enabled without these
//! dev/test observations.

use serde::{Deserialize, Serialize};

/// Actual work performed from headless render entry through a completed submit.
///
/// A skipped render produces no observation; draining twice yields
/// `None`, and the next actual render discards any undrained prior frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkCounts {
    /// Delta of `TextRasterizer::shape_call_count`: cache-miss shaping calls.
    pub layout: u64,
    /// Sum of per-instance widget raster-count deltas: entered invocations,
    /// whether or not rasterization or resource admission succeeds.
    pub raster: u64,
    /// Widget RGBA `queue.write_texture` submissions, after resource admission.
    pub upload: u64,
}
