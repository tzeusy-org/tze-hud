//! # channels
//!
//! The compositor's frame-ready watch: the cross-thread signal the windowed
//! runtime uses to present.

use tokio::sync::watch;

/// FrameReadySignal: the compositor thread tells the main thread a frame is
/// ready to be presented. Uses `tokio::sync::watch` — only the latest value
/// matters.
///
/// Value `true` means "compositor has submitted GPU work, call surface.present()".
/// Value `false` is the initial state (no frame ready yet).
pub(crate) type FrameReadyTx = watch::Sender<bool>;
pub(crate) type FrameReadyRx = watch::Receiver<bool>;

/// Create a FrameReadySignal pair (sender lives on compositor thread, receiver
/// on main thread).
pub(crate) fn frame_ready_channel() -> (FrameReadyTx, FrameReadyRx) {
    watch::channel(false)
}
