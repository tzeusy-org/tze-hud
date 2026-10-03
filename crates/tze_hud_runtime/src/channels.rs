//! # channels
//!
//! The two cross-thread signals the windowed runtime uses: the compositor's
//! frame-ready watch and the OS input event queue payload.

use tokio::sync::watch;

/// Capacity of the OS input event queue; the oldest event is dropped when full.
pub const INPUT_EVENT_CAPACITY: usize = 256;

/// FrameReadySignal: the compositor thread tells the main thread a frame is
/// ready to be presented. Uses `tokio::sync::watch` — only the latest value
/// matters.
///
/// Value `true` means "compositor has submitted GPU work, call surface.present()".
/// Value `false` is the initial state (no frame ready yet).
pub type FrameReadyTx = watch::Sender<bool>;
pub type FrameReadyRx = watch::Receiver<bool>;

/// Create a FrameReadySignal pair (sender lives on compositor thread, receiver
/// on main thread).
pub fn frame_ready_channel() -> (FrameReadyTx, FrameReadyRx) {
    watch::channel(false)
}

/// An OS input event drained from the winit event loop.
///
/// Nothing reads the queue yet; it stays until the portal driver stops
/// constructing it.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InputEvent {
    /// Monotonic timestamp of when the event was received (nanoseconds from
    /// an arbitrary, stable origin). Not wall-clock time.
    pub timestamp_ns: u64,
    pub kind: InputEventKind,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum InputEventKind {
    KeyPress { key: u32 },
    KeyRelease { key: u32 },
    PointerMove { x: f32, y: f32 },
    PointerPress { x: f32, y: f32, button: u8 },
    PointerRelease { x: f32, y: f32, button: u8 },
    Resize { width: u32, height: u32 },
    CloseRequested,
}
