//! `GET /admin/screenshot`: a PNG of the HUD's own rendered frame.
//!
//! The network thread asks the compositor thread for one frame over a channel
//! ([`CaptureEndpoint`] -> [`CaptureInbox`]); the compositor renders it
//! offscreen only when asked (no per-frame readback, no change to its idle
//! gate) and the PNG is encoded back on the network side, off the compositor
//! thread. One capture is in flight at a time.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc, oneshot};
use tze_hud_compositor::{CaptureError, CapturedFrame};

use crate::http::{OperatorCode, OperatorError, Response};

/// How long the compositor has to answer before the request fails.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(3);

/// Largest PNG the endpoint will return.
pub const MAX_PNG_BYTES: usize = 32 * 1024 * 1024;

/// One capture request; the compositor answers on `reply`.
pub struct CaptureRequest {
    /// Display index as `/admin/status` lists them; 0 is the primary.
    pub display: usize,
    pub reply: oneshot::Sender<Result<CapturedFrame, CaptureError>>,
}

/// Compositor-side end: requests waiting to be served. Cloneable so a
/// restarted compositor thread (mode switch) keeps draining the same queue.
#[derive(Clone)]
pub struct CaptureInbox(Arc<std::sync::Mutex<mpsc::UnboundedReceiver<CaptureRequest>>>);

impl CaptureInbox {
    /// An inbox no endpoint feeds (runtimes without an admin screenshot).
    pub fn detached() -> Self {
        capture_channel(|| {}).1
    }

    /// Next request whose requester is still waiting; abandoned (timed-out)
    /// requests are dropped without rendering.
    pub fn next_live(&self) -> Option<CaptureRequest> {
        let mut rx = self.0.lock().unwrap_or_else(|e| e.into_inner());
        while let Ok(req) = rx.try_recv() {
            if !req.reply.is_closed() {
                return Some(req);
            }
        }
        None
    }
}

/// Network-side end, cheap to clone. At most one capture runs at a time.
#[derive(Clone)]
pub struct CaptureEndpoint {
    tx: mpsc::UnboundedSender<CaptureRequest>,
    /// Wakes the compositor thread so it serves the request promptly.
    wake: Arc<dyn Fn() + Send + Sync>,
    in_flight: Arc<Semaphore>,
}

impl fmt::Debug for CaptureEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureEndpoint").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenshotError {
    /// Another capture is running.
    Busy,
    /// No display to read from, or the compositor did not answer in time.
    Unavailable(&'static str),
    /// Frame or PNG exceeds the size limits.
    TooLarge,
    /// The GPU readback or PNG encode failed.
    Failed(String),
    /// The requested display index is not connected.
    NoSuchDisplay(usize),
}

impl ScreenshotError {
    pub fn response(&self) -> Response {
        let (status, code, hint) = match self {
            Self::Busy => (
                429,
                OperatorCode::Busy,
                "a screenshot is already being taken; retry shortly".to_owned(),
            ),
            Self::Unavailable(why) => (503, OperatorCode::Unavailable, (*why).to_owned()),
            Self::TooLarge => (
                422,
                OperatorCode::TooLarge,
                "the display is larger than the screenshot limit".to_owned(),
            ),
            Self::Failed(why) => (503, OperatorCode::Unavailable, why.clone()),
            Self::NoSuchDisplay(i) => (
                404,
                OperatorCode::BadRequest,
                format!("no display {i}; /admin/status lists the connected displays"),
            ),
        };
        Response::operator_error(status, &OperatorError::new(code, hint))
    }
}

impl From<CaptureError> for ScreenshotError {
    fn from(e: CaptureError) -> Self {
        match e {
            CaptureError::TooLarge { .. } => Self::TooLarge,
            CaptureError::NoSuchDisplay(i) => Self::NoSuchDisplay(i),
            other => Self::Failed(other.to_string()),
        }
    }
}

/// A connected endpoint/inbox pair. `wake` is called after each request is
/// queued and must wake the thread that drains the inbox.
pub fn capture_channel(wake: impl Fn() + Send + Sync + 'static) -> (CaptureEndpoint, CaptureInbox) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        CaptureEndpoint {
            tx,
            wake: Arc::new(wake),
            in_flight: Arc::new(Semaphore::new(1)),
        },
        CaptureInbox(Arc::new(std::sync::Mutex::new(rx))),
    )
}

impl CaptureEndpoint {
    /// Ask the compositor for display `display`'s frame (0 = primary) and
    /// return it PNG-encoded.
    pub async fn capture_png(&self, display: usize) -> Result<Vec<u8>, ScreenshotError> {
        let _permit = self
            .in_flight
            .try_acquire()
            .map_err(|_| ScreenshotError::Busy)?;
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(CaptureRequest { display, reply })
            .map_err(|_| ScreenshotError::Unavailable("the compositor is not running"))?;
        (self.wake)();
        let frame = match tokio::time::timeout(CAPTURE_TIMEOUT, answer).await {
            Ok(Ok(frame)) => frame?,
            Ok(Err(_)) | Err(_) => {
                return Err(ScreenshotError::Unavailable(
                    "the compositor did not answer in time",
                ));
            }
        };
        tokio::task::spawn_blocking(move || encode_png(&frame))
            .await
            .map_err(|e| ScreenshotError::Failed(e.to_string()))?
    }
}

/// Encode a captured RGBA8 frame as PNG, within [`MAX_PNG_BYTES`].
pub fn encode_png(frame: &CapturedFrame) -> Result<Vec<u8>, ScreenshotError> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::{ExtendedColorType, ImageEncoder};

    let expected = frame.width as usize * frame.height as usize * 4;
    if frame.rgba.len() != expected {
        return Err(ScreenshotError::Failed(format!(
            "frame buffer is {} bytes, expected {expected}",
            frame.rgba.len()
        )));
    }
    let mut out = Vec::new();
    PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Sub)
        .write_image(
            &frame.rgba,
            frame.width,
            frame.height,
            ExtendedColorType::Rgba8,
        )
        .map_err(|e| ScreenshotError::Failed(e.to_string()))?;
    if out.len() > MAX_PNG_BYTES {
        return Err(ScreenshotError::TooLarge);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_2x2() -> CapturedFrame {
        CapturedFrame {
            width: 2,
            height: 2,
            rgba: vec![
                255, 0, 0, 255, 0, 255, 0, 128, //
                0, 0, 255, 0, 10, 20, 30, 40,
            ],
        }
    }

    #[test]
    fn png_round_trips_the_rgba_buffer() {
        let png = encode_png(&frame_2x2()).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (2, 2));
        assert_eq!(decoded.into_raw(), frame_2x2().rgba);
    }

    #[test]
    fn mismatched_buffer_is_an_error_not_a_panic() {
        let mut f = frame_2x2();
        f.rgba.pop();
        assert!(matches!(encode_png(&f), Err(ScreenshotError::Failed(_))));
    }

    #[test]
    fn errors_map_to_operator_statuses() {
        assert_eq!(ScreenshotError::Busy.response().status, 429);
        assert_eq!(ScreenshotError::Unavailable("x").response().status, 503);
        assert_eq!(ScreenshotError::TooLarge.response().status, 422);
        let too_big: ScreenshotError = CaptureError::TooLarge {
            width: 9000,
            height: 1,
        }
        .into();
        assert_eq!(too_big, ScreenshotError::TooLarge);
    }

    /// A stand-in compositor thread: drains the inbox when woken.
    fn fake_compositor(
        answer: impl Fn() -> Result<CapturedFrame, CaptureError> + Send + 'static,
    ) -> CaptureEndpoint {
        let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
        let (endpoint, inbox) = capture_channel(move || {
            let _ = wake_tx.send(());
        });
        std::thread::spawn(move || {
            while wake_rx.recv().is_ok() {
                if let Some(req) = inbox.next_live() {
                    let _ = req.reply.send(answer());
                }
            }
        });
        endpoint
    }

    #[tokio::test]
    async fn capture_round_trips_through_the_compositor_thread() {
        let endpoint = fake_compositor(|| Ok(frame_2x2()));
        let png = endpoint.capture_png(0).await.unwrap();
        assert_eq!(&png[..4], b"\x89PNG");
        // The permit is released: a second capture succeeds.
        assert!(endpoint.capture_png(0).await.is_ok());
    }

    #[tokio::test]
    async fn compositor_errors_pass_through() {
        let endpoint = fake_compositor(|| {
            Err(CaptureError::TooLarge {
                width: 9000,
                height: 9000,
            })
        });
        assert_eq!(
            endpoint.capture_png(0).await,
            Err(ScreenshotError::TooLarge)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn silent_compositor_times_out_as_unavailable() {
        let (endpoint, _inbox) = capture_channel(|| {});
        let err = endpoint.capture_png(0).await.unwrap_err();
        assert!(matches!(err, ScreenshotError::Unavailable(_)), "{err:?}");
        assert_eq!(err.response().status, 503);
    }

    #[tokio::test(start_paused = true)]
    async fn only_one_capture_is_in_flight_and_abandoned_requests_are_skipped() {
        let (endpoint, inbox) = capture_channel(|| {});
        let first = tokio::spawn({
            let e = endpoint.clone();
            async move { e.capture_png(0).await }
        });
        tokio::task::yield_now().await;
        // While the first waits, a second is refused outright.
        assert_eq!(endpoint.capture_png(0).await, Err(ScreenshotError::Busy));
        // The first times out; its queued request is now abandoned.
        assert!(matches!(
            first.await.unwrap(),
            Err(ScreenshotError::Unavailable(_))
        ));
        assert!(inbox.next_live().is_none());
    }
}
