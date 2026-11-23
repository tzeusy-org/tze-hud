//! Bounded, content-free startup observations shared by widget/font producers.
//! These records describe a single runtime startup, not a persistent event log.

use serde::Serialize;

const MAX_STARTUP_WARNINGS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningSource {
    WidgetBundle,
    Font,
}

/// Codes and fallback families are producer-owned constants. Subjects identify
/// configured ordinals or fixed font roles; never paths or configuration values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StartupWarning {
    pub source: WarningSource,
    pub code: &'static str,
    pub subject: Option<String>,
    pub fallback: Option<&'static str>,
}

#[derive(Clone, Debug, Default)]
pub struct StartupWarnings {
    items: Vec<StartupWarning>,
    /// Dropped events, not a claim about distinct warnings beyond the cap.
    omitted_events: u64,
}

impl StartupWarnings {
    pub fn items(&self) -> &[StartupWarning] {
        &self.items
    }
    pub fn omitted_events(&self) -> u64 {
        self.omitted_events
    }

    pub fn push(&mut self, warning: StartupWarning) {
        if self.items.contains(&warning) {
            return;
        }
        if self.items.len() == MAX_STARTUP_WARNINGS {
            self.omitted_events = self.omitted_events.saturating_add(1);
        } else {
            self.items.push(warning);
        }
    }

    pub fn extend(&mut self, other: Self) {
        self.omitted_events = self.omitted_events.saturating_add(other.omitted_events);
        for warning in other.items {
            self.push(warning);
        }
    }

    pub fn widget(&mut self, code: &'static str, subject: String) {
        self.push(StartupWarning {
            source: WarningSource::WidgetBundle,
            code,
            subject: Some(subject),
            fallback: None,
        });
    }

    pub fn font(&mut self, code: &'static str, subject: String, fallback: Option<&'static str>) {
        self.push(StartupWarning {
            source: WarningSource::Font,
            code,
            subject: Some(subject),
            fallback,
        });
    }
}
