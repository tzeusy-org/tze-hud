//! # tze_hud_compositor
//!
//! wgpu compositor for tze_hud. Renders the scene graph to a native window
//! or headless offscreen texture.
//! Satisfies DR-V2: Headless rendering.
//! Satisfies DR-V6: No physical GPU required (llvmpipe/WARP).

pub(crate) mod display;
pub mod fonts;
pub mod markdown;
pub mod overflow;
pub(crate) mod pipeline;
pub mod renderer;
pub mod surface;
pub(crate) mod text;
pub mod vertical_flow;
pub mod widget;

pub use display::{DisplayLayout, DisplayRect, FrameTarget, normalize_display_name};
pub use markdown::{MarkdownCache, MarkdownTokens, ParsedMarkdown};

pub use renderer::capture::{CaptureError, CapturedFrame};
pub use renderer::{
    ComposerVisualLayoutHandle, Compositor, CompositorAdapterInfo, CompositorDegradationPolicy,
    CompositorError, FocusRingOwner, FocusRingOwnerHandle, LocalComposerState,
    LocalComposerStateHandle, PortalViewerEchoQueue, ResizeGripHoverHandle, SystemCardKind,
    SystemCardModel, TileCloseHoverHandle, ViewerEchoAppend,
};
pub use surface::{CompositorSurface, HeadlessSurface, SurfaceFactory, WindowSurface};
pub use text::TextItem;

#[cfg(test)]
#[path = "../tests/common/pixels.rs"]
mod test_pixels;
