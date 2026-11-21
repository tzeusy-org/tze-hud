//! Owner-pressed notification links. Call only after all scene/state guards drop.
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tze_hud_scene::render_wake::RenderWakeNotifier;
use tze_hud_scene::{LinkFeedbackState, LinkFeedbackTarget};

/// A worker's result: 0 while it runs, then [`OPENED`] or [`FAILED`].
const OPENED: u8 = 1;
const FAILED: u8 = 2;

#[cfg(test)]
type TestLinkWorker = std::sync::Arc<dyn Fn(String) -> Result<(), String> + Send + Sync>;

#[derive(Default)]
pub(crate) struct LinkOpener {
    /// Accepted runtime handoffs, not successful browser navigations. No URL
    /// is retained in diagnostics; admission already applied the rate limit.
    pub(crate) launches: Arc<AtomicU64>,
    /// Wakes the main loop once when a worker finishes.
    pub(crate) wake: Option<RenderWakeNotifier>,
    #[cfg(test)]
    pub(crate) worker: Option<TestLinkWorker>,
    #[cfg(test)]
    pub(crate) workers: Vec<std::thread::JoinHandle<()>>,
    #[cfg(test)]
    pub(crate) fail_spawn: bool,
}

impl LinkOpener {
    /// Hand `url` to a worker. Returns its result flag, set once and followed
    /// by one main-loop wake, or `None` when no worker could start.
    pub(crate) fn launch(&mut self, url: String) -> Option<Arc<AtomicU8>> {
        #[cfg(test)]
        if self.fail_spawn {
            tracing::warn!("notification link worker spawn failed (injected)");
            return None;
        }
        #[cfg(test)]
        let worker = self.worker.clone();
        let done = Arc::new(AtomicU8::new(0));
        let (flag, wake) = (Arc::clone(&done), self.wake.clone());
        let spawned = std::thread::Builder::new()
            .name("notification-link".into())
            .spawn(move || {
                #[cfg(test)]
                let result = worker.map_or_else(|| platform_open(&url), |open| open(url.clone()));
                #[cfg(not(test))]
                let result = platform_open(&url);
                if let Err(error) = &result {
                    // Never include the target: URLs may contain query credentials.
                    tracing::warn!(%error, "notification browser handoff failed");
                }
                flag.store(
                    if result.is_ok() { OPENED } else { FAILED },
                    Ordering::Release,
                );
                if let Some(wake) = wake {
                    wake.notify();
                }
            });
        match spawned {
            Ok(_worker) => {
                self.launches.fetch_add(1, Ordering::Relaxed);
                #[cfg(test)]
                self.workers.push(_worker);
                Some(done)
            }
            Err(error) => {
                tracing::warn!(%error, "notification link worker spawn failed");
                None
            }
        }
    }
}

/// The newest pressed link's local feedback: "Opening…" until its worker
/// reports, then the result for the hold time. Nothing polls: the worker's
/// one wake and the hold deadline drive [`Self::advance`].
#[derive(Debug)]
pub(crate) struct LinkFeedback {
    pub(crate) target: LinkFeedbackTarget,
    pub(crate) state: LinkFeedbackState,
    done: Option<Arc<AtomicU8>>,
    /// When the shown result ends; `None` while opening.
    pub(crate) until: Option<Instant>,
    /// The scene shows the current state.
    pub(crate) drawn: bool,
}

impl LinkFeedback {
    /// A counted launch opens; a worker that never started has failed.
    pub(crate) fn start(
        target: LinkFeedbackTarget,
        done: Option<Arc<AtomicU8>>,
        now: Instant,
        hold: Duration,
    ) -> Self {
        let state = if done.is_some() {
            LinkFeedbackState::Opening
        } else {
            LinkFeedbackState::Failed
        };
        Self {
            target,
            state,
            until: done.is_none().then_some(now + hold),
            done,
            drawn: false,
        }
    }

    /// Take a finished worker's result. Returns whether the state changed.
    pub(crate) fn advance(&mut self, now: Instant, hold: Duration) -> bool {
        let result = self.done.as_ref().map(|d| d.load(Ordering::Acquire));
        let state = match result {
            Some(OPENED) => LinkFeedbackState::Opened,
            Some(FAILED) => LinkFeedbackState::Failed,
            _ => return false,
        };
        self.done = None;
        self.state = state;
        self.until = Some(now + hold);
        self.drawn = false;
        true
    }

    pub(crate) fn expired(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now >= until)
    }
}

/// `link.feedback.ms`, default 1500.
pub(crate) fn feedback_hold(tokens: &std::collections::HashMap<String, String>) -> Duration {
    let ms = tokens
        .get("link.feedback.ms")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(1500.0);
    Duration::from_millis(ms.min(u32::MAX as f64).round() as u64)
}

#[cfg(not(windows))]
fn platform_open(_url: &str) -> Result<(), String> {
    tracing::info!("notification browser handoff is supported on Windows only");
    Ok(())
}

#[cfg(windows)]
fn platform_open(url: &str) -> Result<(), String> {
    use windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
    };
    use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    // SAFETY: this dedicated worker has no pre-existing apartment. Its successful
    // initialization is balanced even if ShellExecuteExW fails.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok() }
        .map_err(|e| format!("COM initialization: {e}"))?;
    let _com = Com;
    let encoded: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    let mut request = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("open"),
        lpFile: PCWSTR(encoded.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: the owned encoded URL lives through this call. No arguments or
    // shell command are constructed; any blocking shell work stays on the worker.
    unsafe { ShellExecuteExW(&mut request) }.map_err(|e| format!("ShellExecuteExW open: {e}"))
}
