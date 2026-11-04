//! Owner-controlled publication defaults, shared by both network planes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

pub const DEFAULT_ANSWER_WINDOW_MS: u32 = 60_000;
pub const MAX_ANSWER_WINDOW_MS: u32 = 300_000;

/// A live read handle. Publications snapshot it once at admission; changing it
/// neither wakes rendering nor retimes already accepted content.
#[derive(Clone, Debug)]
pub struct PublicationDefaults {
    answer_window_ms: Arc<AtomicU32>,
}

impl Default for PublicationDefaults {
    fn default() -> Self {
        Self::new(DEFAULT_ANSWER_WINDOW_MS)
    }
}

impl PublicationDefaults {
    pub fn new(answer_window_ms: u32) -> Self {
        assert!((1..=MAX_ANSWER_WINDOW_MS).contains(&answer_window_ms));
        Self {
            answer_window_ms: Arc::new(AtomicU32::new(answer_window_ms)),
        }
    }

    pub fn answer_window_ms(&self) -> u32 {
        self.answer_window_ms.load(Ordering::Acquire)
    }

    /// The owner calls this only after a successful validated config save.
    pub fn set_answer_window_ms(&self, value: u32) {
        assert!((1..=MAX_ANSWER_WINDOW_MS).contains(&value));
        self.answer_window_ms.store(value, Ordering::Release);
    }
}
