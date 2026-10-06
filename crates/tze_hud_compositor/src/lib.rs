//! # tze_hud_compositor
//!
//! wgpu compositor for tze_hud. Renders the scene graph to a native window
//! or headless offscreen texture.
//! Satisfies DR-V2: Headless rendering.
//! Satisfies DR-V6: No physical GPU required (llvmpipe/WARP).

pub mod display;
pub mod fonts;
pub mod markdown;
pub mod overflow;
pub mod pipeline;
pub mod renderer;
pub mod surface;
pub mod text;
pub mod vertical_flow;
pub mod widget;

pub use display::{DisplayLayout, DisplayRect, FrameTarget, normalize_display_name};
pub use fonts::{
    BUNDLED_FONT_FACE_COUNT, FontConfig, ResolvedFonts, build_font_system, bundled_font_sources,
    bundled_font_system,
};
pub use markdown::{
    MarkdownCache, MarkdownPrimer, MarkdownTokens, ParsedMarkdown, PrimeJob, StyleAttr, StyledSpan,
};
pub use overflow::{
    ELLIPSIS, TruncationResult, TruncationViewport, truncate_for_ellipsis, truncate_tail_anchored,
};
pub use pipeline::{RoundedRectDrawCmd, TexturedRectVertex};
pub use renderer::capture::{CaptureError, CapturedFrame, MAX_CAPTURE_DIM};
pub use renderer::{
    ComposerVisualLayoutHandle, Compositor, CompositorAdapterInfo, CompositorDegradationPolicy,
    CompositorError, FocusRingOwner, FocusRingOwnerHandle, ImageTextureEntry, LocalComposerState,
    LocalComposerStateHandle, PortalViewerEchoQueue, ResizeGripHoverHandle, SystemCardKind,
    SystemCardModel, TileCloseHoverHandle, ViewerEchoAppend, ViewerEchoEntry, ViewerEchoStore,
};
pub use surface::{
    CompositorFrame, CompositorSurface, HeadlessSurface, SurfaceFactory, WindowSurface,
};
pub use text::{LINE_HEIGHT_MULTIPLIER, StyledRunItem, TextItem, TextRasterizer};
pub use widget::{WidgetRenderer, interpolate_param};
