//! Owner-controlled publication defaults, shared by both network planes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

pub const DEFAULT_ANSWER_WINDOW_MS: u32 = 60_000;
pub const MAX_ANSWER_WINDOW_MS: u32 = 300_000;

/// Where a new question card appears (`[runtime] question_placement`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuestionPlacement {
    /// The monitor of the window the owner is working in.
    #[default]
    Focused,
    /// The monitor under the pointer.
    Pointer,
    /// The notification display (primary unless `[displays]` moves it).
    Fixed,
}

impl QuestionPlacement {
    pub const ALL: [Self; 3] = [Self::Focused, Self::Pointer, Self::Fixed];

    /// The `[runtime] question_placement` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Focused => "focused",
            Self::Pointer => "pointer",
            Self::Fixed => "fixed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == value)
    }
}

/// A live read handle. Publications snapshot it once at admission; changing it
/// neither wakes rendering nor retimes already accepted content.
#[derive(Clone, Debug)]
pub struct PublicationDefaults {
    answer_window_ms: Arc<AtomicU32>,
    question_placement: Arc<AtomicU8>,
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
            question_placement: Arc::default(),
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

    /// Read once per presented question; a change moves no visible card.
    pub fn question_placement(&self) -> QuestionPlacement {
        QuestionPlacement::ALL[usize::from(self.question_placement.load(Ordering::Acquire))]
    }

    pub fn set_question_placement(&self, value: QuestionPlacement) {
        self.question_placement
            .store(value as u8, Ordering::Release);
    }
}
