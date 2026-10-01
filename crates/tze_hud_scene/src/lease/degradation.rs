//! Render degradation level shared by the runtime controller and the compositor.
//!
//! There is one fallback. When frame times stay over budget the runtime
//! switches to [`DegradationLevel::Simplified`]; when they recover it switches
//! back. Leases and the scene graph are never changed by degradation — only
//! how the frame is drawn.

/// How the compositor should draw the current frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum DegradationLevel {
    /// Full quality.
    #[default]
    Nominal,
    /// Cheaper frame: large textures downscaled, translucent fills drawn
    /// opaque, and animations/transitions snapped to their end state.
    Simplified,
}
