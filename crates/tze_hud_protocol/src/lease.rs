//! Lease management helpers for the session stream (RFC 0005 §3.1, §3.2, §5.3).
//!
//! This module provides:
//!
//! - [`LeaseCorrelationCache`] — per-session deduplication cache for lease
//!   operations.  Lease operations use the client `sequence` number as the
//!   correlation key (RFC 0005 §5.3): if an agent retransmits with the same
//!   sequence number the server returns the previously cached response without
//!   re-applying the operation.
//!
//! - Priority enforcement helpers per the lease-governance spec.

use std::collections::HashMap;

// ─── Capacity constant ────────────────────────────────────────────────────────

/// Default capacity for the per-session lease correlation cache.
///
/// Holds the last 256 lease-operation responses per session.  An agent
/// sending more than 256 lease requests without receiving ACKs is operating
/// far outside normal patterns; oldest entries are evicted when the cap is
/// hit.
pub const DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY: usize = 256;

// ─── Retransmit correlation (RFC 0005 §5.3) ──────────────────────────────────

/// Cached server response to a single lease operation.
///
/// Keyed by the **client sequence number** that carried the original request.
/// On retransmit the server looks up the sequence and replays the cached
/// response without re-applying the operation.
#[derive(Debug, Clone)]
pub struct CachedLeaseResponse {
    /// Whether the operation was granted.
    pub granted: bool,
    /// Granted lease ID bytes (16-byte UUIDv7).  Empty if denied.
    pub lease_id: Vec<u8>,
    /// Granted TTL in milliseconds.  Zero if denied.
    pub granted_ttl_ms: u64,
    /// Granted priority level.
    pub granted_priority: u32,
    /// Granted capability strings.
    pub granted_capabilities: Vec<String>,
    /// Human-readable denial reason (empty if granted).
    pub deny_reason: String,
    /// Machine-readable denial code (empty if granted).
    pub deny_code: String,
}

/// Per-session cache of recent lease-operation responses, keyed by the
/// client-side sequence number that originated the request.
///
/// Cache capacity is capped at `capacity` entries; oldest entries are evicted
/// when the cap is exceeded (LRU-approximated via insertion-order VecDeque).
#[derive(Debug)]
pub struct LeaseCorrelationCache {
    /// Maps client_sequence → cached response.
    entries: HashMap<u64, CachedLeaseResponse>,
    /// Insertion-order list of sequence numbers, for LRU eviction.
    order: std::collections::VecDeque<u64>,
    /// Maximum number of cached entries.
    capacity: usize,
}

impl LeaseCorrelationCache {
    /// Create a new cache with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: std::collections::VecDeque::new(),
            capacity,
        }
    }

    /// Look up the cached response for `client_sequence`.  Returns `None` if
    /// this sequence has not been seen before (i.e. it is a fresh request, not
    /// a retransmit).
    pub fn get(&self, client_sequence: u64) -> Option<&CachedLeaseResponse> {
        self.entries.get(&client_sequence)
    }

    /// Store a response for `client_sequence`.  If the cache is at capacity,
    /// evicts the oldest entry (in insertion order).
    ///
    /// A capacity of 0 is a no-op (nothing is ever cached).
    pub fn insert(&mut self, client_sequence: u64, response: CachedLeaseResponse) {
        if self.capacity == 0 {
            return;
        }
        // If the key already existed, `insert` returns Some(old_value).
        // In that case the value is updated in-place; the order queue is unchanged.
        if self.entries.insert(client_sequence, response).is_some() {
            return;
        }
        // New entry: record insertion order, then evict oldest if over capacity.
        self.order.push_back(client_sequence);
        if self.order.len() > self.capacity {
            if let Some(evict_seq) = self.order.pop_front() {
                self.entries.remove(&evict_seq);
            }
        }
    }
}

// ─── Priority enforcement (lease-governance spec §Priority Assignment) ───────

/// Maximum valid agent-assignable priority (inclusive).  Values > 4 are
/// clamped to 4 at the enforcement boundary.
pub const MAX_LEASE_PRIORITY: u32 = 4;

/// Compute the effective lease priority to grant, enforcing spec rules:
///
/// - Priority 0 is reserved for system/chrome; agents requesting 0 receive 2.
/// - Priority 1 requires the `lease:priority:1` capability; without it the
///   agent receives 2.
/// - Priorities 2–4 are granted as-is.
/// - Any value > 4 is clamped to 4 (lowest priority) before evaluation.
///
/// This is a pure function; it does not access the session registry.
pub fn effective_priority(requested: u32, granted_capabilities: &[String]) -> u32 {
    // Clamp out-of-range values to the lowest valid agent priority.
    let requested = requested.min(MAX_LEASE_PRIORITY);
    match requested {
        0 => 2, // Priority 0 reserved for runtime-internal leases
        1 => {
            // Requires explicit capability grant
            let has_prio1 = granted_capabilities.iter().any(|c| c == "lease:priority:1");
            if has_prio1 { 1 } else { 2 }
        }
        p => p,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── LeaseCorrelationCache tests ─────────────────────────────────────────

    #[test]
    fn test_correlation_cache_miss_on_new_sequence() {
        let cache = LeaseCorrelationCache::new(16);
        assert!(
            cache.get(42).is_none(),
            "New sequence should be a cache miss"
        );
    }

    #[test]
    fn test_correlation_cache_hit_after_insert() {
        let mut cache = LeaseCorrelationCache::new(16);
        let resp = CachedLeaseResponse {
            granted: true,
            lease_id: vec![0u8; 16],
            granted_ttl_ms: 60_000,
            granted_priority: 2,
            granted_capabilities: vec!["create_tile".to_string()],
            deny_reason: String::new(),
            deny_code: String::new(),
        };
        cache.insert(3, resp.clone());
        let hit = cache.get(3).unwrap();
        assert!(hit.granted);
        assert_eq!(hit.granted_ttl_ms, 60_000);
    }

    #[test]
    fn test_correlation_cache_evicts_oldest_when_full() {
        let mut cache = LeaseCorrelationCache::new(3);

        for seq in 1u64..=3 {
            cache.insert(
                seq,
                CachedLeaseResponse {
                    granted: true,
                    lease_id: vec![seq as u8; 16],
                    granted_ttl_ms: 1000,
                    granted_priority: 2,
                    granted_capabilities: Vec::new(),
                    deny_reason: String::new(),
                    deny_code: String::new(),
                },
            );
        }
        assert!(
            cache.get(1).is_some(),
            "seq=1 should be present before eviction"
        );

        // Insert a 4th entry — seq=1 should be evicted (oldest).
        cache.insert(
            4,
            CachedLeaseResponse {
                granted: true,
                lease_id: vec![4u8; 16],
                granted_ttl_ms: 1000,
                granted_priority: 2,
                granted_capabilities: Vec::new(),
                deny_reason: String::new(),
                deny_code: String::new(),
            },
        );

        assert!(cache.get(1).is_none(), "seq=1 should have been evicted");
        assert!(cache.get(2).is_some(), "seq=2 should still be present");
        assert!(cache.get(3).is_some(), "seq=3 should still be present");
        assert!(cache.get(4).is_some(), "seq=4 should be present");
    }

    #[test]
    fn test_correlation_cache_zero_capacity_is_noop() {
        let mut cache = LeaseCorrelationCache::new(0);
        cache.insert(
            1,
            CachedLeaseResponse {
                granted: true,
                lease_id: vec![1u8; 16],
                granted_ttl_ms: 1000,
                granted_priority: 2,
                granted_capabilities: Vec::new(),
                deny_reason: String::new(),
                deny_code: String::new(),
            },
        );
        assert!(
            cache.get(1).is_none(),
            "capacity=0 should never store anything"
        );
    }

    #[test]
    fn test_correlation_cache_overwrite_keeps_order() {
        let mut cache = LeaseCorrelationCache::new(3);

        // Insert seq=1, seq=2, then overwrite seq=1.
        cache.insert(
            1,
            CachedLeaseResponse {
                granted: true,
                lease_id: vec![1u8; 16],
                granted_ttl_ms: 1000,
                granted_priority: 2,
                granted_capabilities: Vec::new(),
                deny_reason: String::new(),
                deny_code: String::new(),
            },
        );
        cache.insert(
            2,
            CachedLeaseResponse {
                granted: true,
                lease_id: vec![2u8; 16],
                granted_ttl_ms: 1000,
                granted_priority: 2,
                granted_capabilities: Vec::new(),
                deny_reason: String::new(),
                deny_code: String::new(),
            },
        );

        // Overwrite seq=1 (should not change insertion order)
        cache.insert(
            1,
            CachedLeaseResponse {
                granted: false,
                lease_id: Vec::new(),
                granted_ttl_ms: 0,
                granted_priority: 2,
                granted_capabilities: Vec::new(),
                deny_reason: "overwritten".to_string(),
                deny_code: "TEST".to_string(),
            },
        );

        // Updated value is returned
        let hit = cache.get(1).unwrap();
        assert!(!hit.granted);
        assert_eq!(hit.deny_reason, "overwritten");
    }

    // ─── Priority enforcement tests ──────────────────────────────────────────

    #[test]
    fn test_priority_zero_downgraded_to_two() {
        // Priority 0 is reserved for system/chrome; agent requests must receive 2.
        assert_eq!(effective_priority(0, &[]), 2);
        assert_eq!(
            effective_priority(0, &["lease:priority:1".to_string()]),
            2,
            "Even with prio-1 cap, priority=0 requests must be downgraded to 2"
        );
    }

    #[test]
    fn test_priority_one_without_capability_downgraded_to_two() {
        // Agent requests priority 1 but lacks the capability.
        assert_eq!(effective_priority(1, &[]), 2);
        assert_eq!(
            effective_priority(1, &["create_tile".to_string()]),
            2,
            "Priority 1 without lease:priority:1 cap should give priority 2"
        );
    }

    #[test]
    fn test_priority_one_with_capability_granted() {
        assert_eq!(effective_priority(1, &["lease:priority:1".to_string()]), 1);
    }

    #[test]
    fn test_priority_two_and_above_passed_through() {
        for p in [2u32, 3, 4] {
            assert_eq!(
                effective_priority(p, &[]),
                p,
                "Priority {p} should be passed through unchanged"
            );
        }
    }

    #[test]
    fn test_priority_out_of_range_clamped_to_four() {
        // Values > 4 are out of spec range and should be clamped to 4.
        assert_eq!(
            effective_priority(5, &[]),
            4,
            "Priority 5 (out of range) must be clamped to 4"
        );
        assert_eq!(
            effective_priority(u32::MAX, &[]),
            4,
            "u32::MAX must be clamped to 4"
        );
    }
}
