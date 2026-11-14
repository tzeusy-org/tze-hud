use crate::types::{ContentionPolicy, WidgetPublishRecord, ZonePublishRecord};
use crate::validation::ValidationError;

// ─── Contention policy helper ────────────────────────────────────────────────

/// Accessor trait for the fields that `apply_contention` reads from a publish
/// record.  Implemented by both `ZonePublishRecord` and `WidgetPublishRecord`
/// so the single contention function can serve all three publish entry points.
///
/// This trait is intentionally private to this module.
pub(crate) trait ContentionRecord {
    fn record_publisher_namespace(&self) -> &str;
    fn record_merge_key(&self) -> Option<&str>;
}

impl ContentionRecord for ZonePublishRecord {
    fn record_publisher_namespace(&self) -> &str {
        &self.publisher_namespace
    }
    fn record_merge_key(&self) -> Option<&str> {
        self.merge_key.as_deref()
    }
}

impl ContentionRecord for WidgetPublishRecord {
    fn record_publisher_namespace(&self) -> &str {
        &self.publisher_namespace
    }
    fn record_merge_key(&self) -> Option<&str> {
        self.merge_key.as_deref()
    }
}

/// Apply `contention_policy` to `publishes`, inserting `record`.
///
/// # Canonical semantics (all three zone/widget publish entry points use these)
///
/// - `LatestWins` / `Replace` — replace all existing records with the new one.
/// - `Stack { max_depth }` — enforce `max_publishers` per namespace, push the
///   record, then trim so the total stays within `max_depth`. Each trim takes
///   the oldest record `spare` rejects; only when `spare` keeps every record
///   does the oldest go. `max_depth == 0` trims the stack to zero (i.e. the
///   publish is silently discarded); callers that want to reject rather than
///   discard should validate `max_depth` before calling.
/// - `MergeByKey { max_keys }` — replace the same-key entry in place, or add a
///   new key (evicting the oldest when at capacity).
///
/// # Arguments
///
/// - `publishes` — the mutable record list for this zone/widget
/// - `record` — the new publish record to apply
/// - `contention_policy` — the active policy
/// - `max_publishers` — per-namespace publication limit (enforced for `Stack`)
/// - `make_max_publishers_err` — constructs the rejection error when the limit
///   is reached; called with the effective limit.  Zone callers supply
///   `ZoneMaxPublishersReached`; widget callers supply `WidgetMaxPublishersReached`.
/// - `spare` — `Stack` trim preference: records it accepts leave only when
///   every record is spared (notification zones spare unanswered questions).
///
/// Returns the records a `Stack` trim removed, oldest first; replacement and
/// key eviction by the other policies are not trims.
pub(crate) fn apply_contention<R: ContentionRecord>(
    publishes: &mut Vec<R>,
    record: R,
    contention_policy: ContentionPolicy,
    max_publishers: u32,
    make_max_publishers_err: impl Fn(u32) -> ValidationError,
    spare: impl Fn(&R) -> bool,
) -> Result<Vec<R>, ValidationError> {
    let mut trimmed = Vec::new();
    match contention_policy {
        ContentionPolicy::LatestWins => {
            *publishes = vec![record];
        }
        ContentionPolicy::Replace => {
            *publishes = vec![record];
        }
        ContentionPolicy::Stack { max_depth } => {
            // Check publisher count limit before accepting the record.
            let publisher_count = publishes
                .iter()
                .filter(|r| r.record_publisher_namespace() == record.record_publisher_namespace())
                .count() as u32;
            if publisher_count >= max_publishers {
                return Err(make_max_publishers_err(max_publishers));
            }
            publishes.push(record);
            // Trim so the stack stays within max_depth, oldest unspared first.
            // max_depth == 0 trims to zero (the pushed record is removed).
            let max = max_depth as usize;
            while publishes.len() > max {
                let victim = publishes.iter().position(|r| !spare(r)).unwrap_or(0);
                trimmed.push(publishes.remove(victim));
            }
        }
        ContentionPolicy::MergeByKey { max_keys } => {
            let key = record.record_merge_key().unwrap_or("").to_string();
            if let Some(pos) = publishes
                .iter()
                .position(|r| r.record_merge_key().unwrap_or("") == key.as_str())
            {
                publishes[pos] = record;
            } else {
                let max = max_keys as usize;
                if max > 0 && publishes.len() >= max {
                    // At max key capacity — evict the oldest entry so the new key
                    // can take its place.  "Oldest" is the front of the
                    // insertion-ordered Vec (index 0).
                    // Spec: openspec/changes/exemplar-status-bar/tasks.md §2.5
                    //   "oldest evicted, 32 remain"
                    publishes.remove(0);
                }
                publishes.push(record);
            }
        }
    }
    Ok(trimmed)
}
