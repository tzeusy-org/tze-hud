//! Relative widget deadlines become wall time only when a publication presents.
//! Formatting and the next visible change share the same rounded time units.

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
}

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
        if !self.widgets_shown() || self.widget_time_paused() {
            return None;
        }
        let now = self.now_wall_us();
        self.widget_registry
            .instances
            .values()
            .filter(|instance| self.widget_visible(instance))
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
        }
    }

    pub fn valid(&self) -> bool {
        (0..=Self::MAX_MS).contains(&self.ends_in_ms)
    }

    /// Call only after admission, at presentation (including delayed publishes).
    pub fn snapshot(&mut self, now_wall_us: u64) {
        debug_assert!(self.valid());
        self.at_wall_us = Some(now_wall_us.saturating_add(self.ends_in_ms as u64 * 1_000));
    }

    fn remaining_us(&self, now_wall_us: u64) -> u64 {
        self.at_wall_us
            .map_or(0, |at| at.saturating_sub(now_wall_us))
    }

    pub fn display_at(&self, now_wall_us: u64) -> String {
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
    pub fn next_change_wall_us(&self, now_wall_us: u64) -> Option<u64> {
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
}
