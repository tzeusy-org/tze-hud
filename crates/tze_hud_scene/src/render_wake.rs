//! Protocol-neutral notification seam for newly available render work.
//!
//! The scene crate owns only the callback value. The production runtime binds
//! it to its platform event proxy and compositor waiter; lower crates neither
//! depend on winit nor know how the wake is delivered.

use std::fmt;
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct RenderWakeNotifier {
    callback: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
}

impl RenderWakeNotifier {
    pub fn new(callback: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            callback: Some(Arc::new(callback)),
        }
    }

    #[inline]
    pub fn notify(&self) {
        if let Some(callback) = &self.callback {
            callback();
        }
    }

    fn is_wired(&self) -> bool {
        self.callback.is_some()
    }
}

impl fmt::Debug for RenderWakeNotifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenderWakeNotifier")
            .field("wired", &self.is_wired())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::RenderWakeNotifier;

    #[test]
    fn default_notifier_is_a_safe_no_op() {
        RenderWakeNotifier::default().notify();
    }

    #[test]
    fn cloned_notifiers_share_the_project_owned_callback() {
        let calls = Arc::new(AtomicU64::new(0));
        let calls_for_callback = Arc::clone(&calls);
        let notifier = RenderWakeNotifier::new(move || {
            calls_for_callback.fetch_add(1, Ordering::Relaxed);
        });

        notifier.notify();
        notifier.clone().notify();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
