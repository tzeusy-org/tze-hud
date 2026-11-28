//! Relative widget deadlines become wall time only when a publication presents.
//! Formatting and the next visible change share the same rounded time units.
//! A count-up value (an elapsed timer) is "started this long ago" and rounds
//! down, so it never shows time that has not yet passed.

use crate::{SceneGraph, WidgetParameterValue};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetDeadline {
    /// Submitted duration. Kept signed so shared admission can reject negatives.
    pub ends_in_ms: i64,
    /// Runtime snapshot; transports accept only the relative duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_wall_us: Option<u64>,
    /// Counts up from a start `ends_in_ms` before presentation; then
    /// `at_wall_us` is the presentation instant. Set by shared admission
    /// from the declaration, never by a transport.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub counts_up: bool,
}

const SECOND_US: u64 = 1_000_000;
const MINUTE_US: u64 = 60 * SECOND_US;
const HOUR_US: u64 = 60 * MINUTE_US;
const DAY_US: u64 = 24 * HOUR_US;
/// An elapsed timer holds `99 d 23 h` from here and stops waking.
const ELAPSED_HOLD_US: u64 = 100 * DAY_US - HOUR_US;

/// Resolve only presentation values; authoritative deadlines remain absolute.
/// Passing the scene clock here also makes cached text equality time-aware.
pub fn widget_params_at(
    params: &HashMap<String, WidgetParameterValue>,
    now_wall_us: u64,
) -> HashMap<String, WidgetParameterValue> {
    params
        .iter()
        .map(|(name, value)| {
            let value = match value {
                WidgetParameterValue::Deadline(d) => {
                    WidgetParameterValue::String(d.display_at(now_wall_us))
                }
                other => other.clone(),
            };
            (name.clone(), value)
        })
        .collect()
}

impl SceneGraph {
    /// Matches the existing draw policy: all published instances, independent
    /// of tab, while owner-visible and with a drawable box.
    pub fn widget_visible(&self, instance: &crate::WidgetInstance) -> bool {
        if !self.widgets_shown()
            || self
                .widget_registry
                .active_publishes
                .get(&instance.instance_name)
                .is_none_or(|records| records.is_empty())
        {
            return false;
        }
        match self.dock_widget_slot(&instance.instance_name) {
            crate::dock::DockWidgetSlot::Placed(_) => return true,
            crate::dock::DockWidgetSlot::Hidden => return false,
            crate::dock::DockWidgetSlot::Registry => {}
        }
        self.widget_registry
            .drawn_geometry(instance)
            .is_some_and(|geometry| {
                let (_, _, w, h) = crate::resolve_zone_geometry(
                    &geometry,
                    self.display_area.width,
                    self.display_area.height,
                );
                w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0
            })
    }

    pub fn widget_time_paused(&self) -> bool {
        self.overlay.widget_ttls_paused_at_wall_us.is_some()
    }

    /// Only published, drawn widgets may ask for a display-change wake.
    pub fn next_widget_deadline_wall_us(&self) -> Option<u64> {
        self.next_widget_deadline_wall_us_where(|_| true)
    }

    /// Callers can exclude instances whose committed display is not drawable.
    pub fn next_widget_deadline_wall_us_where(
        &self,
        mut drawable: impl FnMut(&crate::WidgetInstance) -> bool,
    ) -> Option<u64> {
        if !self.widgets_shown() || self.widget_time_paused() {
            return None;
        }
        let now = self.now_wall_us();
        self.widget_registry
            .instances
            .values()
            .filter(|instance| drawable(instance) && self.widget_visible(instance))
            .flat_map(|instance| instance.current_params.values())
            .filter_map(|value| match value {
                WidgetParameterValue::Deadline(d) => d.next_change_wall_us(now),
                _ => None,
            })
            .min()
    }

    /// Publication TTLs pause; the deadlines stored in params do not.
    pub fn pause_widget_ttls(&mut self) {
        if self.widget_time_paused() {
            return;
        }
        self.drain_expired_widget_publications();
        self.overlay.widget_ttls_paused_at_wall_us = Some(self.now_wall_us());
    }

    pub fn resume_widget_ttls(&mut self) {
        let Some(start) = self.overlay.widget_ttls_paused_at_wall_us.take() else {
            return;
        };
        let elapsed = self.now_wall_us().saturating_sub(start);
        for record in self.widget_registry.active_publishes.values_mut().flatten() {
            record.ttl_paused_us = record.ttl_paused_us.saturating_add(elapsed);
            record.expires_at_wall_us = record
                .expires_at_wall_us
                .map(|at| at.saturating_add(elapsed));
        }
    }
}

impl WidgetDeadline {
    pub const MAX_MS: i64 = 86_400_000;

    pub fn relative(ends_in_ms: i64) -> Self {
        Self {
            ends_in_ms,
            at_wall_us: None,
            counts_up: false,
        }
    }

    pub fn valid(&self) -> bool {
        (0..=Self::MAX_MS).contains(&self.ends_in_ms)
    }

    /// Call only after admission, at presentation (including delayed publishes).
    pub fn snapshot(&mut self, now_wall_us: u64) {
        debug_assert!(self.valid());
        self.at_wall_us = Some(if self.counts_up {
            now_wall_us
        } else {
            now_wall_us.saturating_add(self.ends_in_ms as u64 * 1_000)
        });
    }

    /// Time since the start, floored at zero (a backwards clock step).
    fn elapsed_us(&self, now_wall_us: u64) -> u64 {
        let ago = self.ends_in_ms.max(0) as u64 * 1_000;
        let at = self.at_wall_us.unwrap_or(now_wall_us);
        if now_wall_us >= at {
            ago.saturating_add(now_wall_us - at)
        } else {
            ago.saturating_sub(at - now_wall_us)
        }
    }

    fn elapsed_display(elapsed_us: u64) -> String {
        let e = elapsed_us.min(ELAPSED_HOLD_US);
        if e < 2 * MINUTE_US {
            let s = e / SECOND_US;
            format!("{}:{:02}", s / 60, s % 60)
        } else if e < HOUR_US {
            format!("{} min", e / MINUTE_US)
        } else if e < DAY_US {
            format!("{} h {:02}", e / HOUR_US, e % HOUR_US / MINUTE_US)
        } else {
            format!("{} d {:02} h", e / DAY_US, e % DAY_US / HOUR_US)
        }
    }

    /// The next elapsed time at which the display changes, if any.
    fn elapsed_next_change(elapsed_us: u64) -> Option<u64> {
        if elapsed_us >= ELAPSED_HOLD_US {
            return None;
        }
        let quantum = if elapsed_us < 2 * MINUTE_US {
            SECOND_US
        } else if elapsed_us < DAY_US {
            MINUTE_US
        } else {
            HOUR_US
        };
        Some((elapsed_us / quantum + 1) * quantum)
    }

    fn remaining_us(&self, now_wall_us: u64) -> u64 {
        self.at_wall_us
            .map_or(0, |at| at.saturating_sub(now_wall_us))
    }

    pub fn display_at(&self, now_wall_us: u64) -> String {
        if self.counts_up {
            return Self::elapsed_display(self.elapsed_us(now_wall_us));
        }
        let seconds = self.remaining_us(now_wall_us).div_ceil(1_000_000);
        if seconds < 120 {
            return format!("{}:{:02}", seconds / 60, seconds % 60);
        }
        let minutes = seconds.div_ceil(60);
        if minutes >= 60 {
            format!("{} h {:02}", minutes / 60, minutes % 60)
        } else {
            format!("{minutes} min")
        }
    }

    /// The first wall instant at which `display_at` changes. Never a periodic
    /// retry, and never due now: expiry has a stable zero display and no wake.
    fn next_change_wall_us(&self, now_wall_us: u64) -> Option<u64> {
        if self.counts_up {
            let elapsed = self.elapsed_us(now_wall_us);
            let next = Self::elapsed_next_change(elapsed)?;
            return Some(now_wall_us.saturating_add(next - elapsed));
        }
        let remaining = self.remaining_us(now_wall_us);
        if remaining == 0 {
            return None;
        }
        let seconds = remaining.div_ceil(1_000_000);
        let quantum = if seconds <= 120 {
            1_000_000
        } else {
            60_000_000
        };
        let until_change = remaining - (remaining.div_ceil(quantum) - 1) * quantum;
        Some(now_wall_us.saturating_add(until_change))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_format_and_next_change_share_rounded_boundaries() {
        for (remaining, label) in [
            (0, "0:00"),
            (1, "0:01"),
            (90_000_000, "1:30"),
            (119_000_000, "1:59"),
            (119_000_001, "2 min"),
            (120_000_000, "2 min"),
            (120_000_001, "3 min"),
            (1_800_000_000, "30 min"),
            (3_600_000_000, "1 h 00"),
            (7_500_000_000, "2 h 05"),
            (86_400_000_000, "24 h 00"),
        ] {
            let deadline = WidgetDeadline {
                ends_in_ms: 0,
                at_wall_us: Some(remaining),
                counts_up: false,
            };
            assert_eq!(deadline.display_at(0), label);
            if let Some(next) = deadline.next_change_wall_us(0) {
                assert!(next > 0);
                assert_eq!(deadline.display_at(next - 1), label);
                assert_ne!(deadline.display_at(next), label);
            } else {
                assert_eq!(remaining, 0);
            }
            assert_eq!(deadline.display_at(remaining.saturating_add(1)), "0:00");
            assert_eq!(deadline.next_change_wall_us(remaining), None);
        }
        let mut deadline = WidgetDeadline::relative(1);
        deadline.snapshot(u64::MAX - 2);
        assert_eq!(deadline.at_wall_us, Some(u64::MAX));
        assert_eq!(deadline.next_change_wall_us(u64::MAX - 2), Some(u64::MAX));
        // Rewinding the injected clock does not mutate or rebase the deadline.
        assert_eq!(deadline.display_at(u64::MAX), "0:00");
        assert_eq!(deadline.display_at(u64::MAX - 2), "0:01");
    }

    /// Elapsed time rounds down; format and next change share the
    /// boundaries at 2 min, 60 min, 24 h and the 100-day hold.
    #[test]
    fn elapsed_format_and_next_change_share_floored_boundaries() {
        let up = |ago_ms: i64| {
            let mut d = WidgetDeadline {
                counts_up: true,
                ..WidgetDeadline::relative(ago_ms)
            };
            d.snapshot(1_000_000_000);
            d
        };
        let start = up(0);
        let at = |elapsed: u64| 1_000_000_000 + elapsed;
        for (elapsed, label) in [
            (0, "0:00"),
            (999_999, "0:00"),
            (42 * SECOND_US, "0:42"),
            (2 * MINUTE_US - 1, "1:59"),
            (2 * MINUTE_US, "2 min"),
            (12 * MINUTE_US + 30 * SECOND_US, "12 min"),
            (HOUR_US - 1, "59 min"),
            (HOUR_US, "1 h 00"),
            (65 * MINUTE_US, "1 h 05"),
            (DAY_US - 1, "23 h 59"),
            (DAY_US, "1 d 00 h"),
            (26 * HOUR_US, "1 d 02 h"),
            (ELAPSED_HOLD_US - 1, "99 d 22 h"),
            (ELAPSED_HOLD_US, "99 d 23 h"),
            (300 * DAY_US, "99 d 23 h"),
        ] {
            assert_eq!(start.display_at(at(elapsed)), label, "{elapsed}");
            match start.next_change_wall_us(at(elapsed)) {
                Some(next) => {
                    assert!(next > at(elapsed));
                    assert_eq!(start.display_at(next - 1), label, "{elapsed}");
                    assert_ne!(start.display_at(next), label, "{elapsed}");
                }
                None => assert!(elapsed >= ELAPSED_HOLD_US, "{elapsed}"),
            }
        }
        // One wake per display change: per second, then per minute, then per hour.
        assert_eq!(start.next_change_wall_us(at(0)), Some(at(SECOND_US)));
        assert_eq!(
            start.next_change_wall_us(at(3 * MINUTE_US)),
            Some(at(4 * MINUTE_US))
        );
        assert_eq!(
            start.next_change_wall_us(at(DAY_US + 1)),
            Some(at(DAY_US + HOUR_US))
        );
        // Started ten minutes ago; a backwards clock step shows 0:00.
        assert_eq!(up(600_000).display_at(at(0)), "10 min");
        assert_eq!(up(1_000).display_at(at(0) - 5 * SECOND_US), "0:00");
        assert_eq!(start.display_at(0), "0:00");
    }
}
