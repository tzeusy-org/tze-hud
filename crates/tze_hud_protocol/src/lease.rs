//! Lease management helpers for the session stream (RFC 0005 §3.1, §3.2, §5.3).
//!
//! This module provides:
//!
//! - [`LeaseCorrelationCache`] — per-session deduplication cache for lease
//!   operations.  Lease operations use the client `sequence` number as the
//!   correlation key (RFC 0005 §5.3): if an agent retransmits with the same
//!   sequence number the server returns the previously cached response without
//!   re-applying the operation.

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

/// Cached reply to a ClaimTile / Hold / Clear request.
///
/// Keyed by the **client sequence number** that carried the original request.
/// On retransmit the server replays the cached reply without re-applying the
/// operation.
pub type CachedLeaseResponse = crate::proto::session::RequestResult;

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
            ok: true,
            lease_id: vec![0u8; 16],
            ttl_ms: 60_000,
            hint: String::new(),
            code: String::new(),
            ..Default::default()
        };
        cache.insert(3, resp.clone());
        let hit = cache.get(3).unwrap();
        assert!(hit.ok);
        assert_eq!(hit.ttl_ms, 60_000);
    }

    #[test]
    fn test_correlation_cache_evicts_oldest_when_full() {
        let mut cache = LeaseCorrelationCache::new(3);

        for seq in 1u64..=3 {
            cache.insert(
                seq,
                CachedLeaseResponse {
                    ok: true,
                    lease_id: vec![seq as u8; 16],
                    ttl_ms: 1000,
                    hint: String::new(),
                    code: String::new(),
                    ..Default::default()
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
                ok: true,
                lease_id: vec![4u8; 16],
                ttl_ms: 1000,
                hint: String::new(),
                code: String::new(),
                ..Default::default()
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
                ok: true,
                lease_id: vec![1u8; 16],
                ttl_ms: 1000,
                hint: String::new(),
                code: String::new(),
                ..Default::default()
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
                ok: true,
                lease_id: vec![1u8; 16],
                ttl_ms: 1000,
                hint: String::new(),
                code: String::new(),
                ..Default::default()
            },
        );
        cache.insert(
            2,
            CachedLeaseResponse {
                ok: true,
                lease_id: vec![2u8; 16],
                ttl_ms: 1000,
                hint: String::new(),
                code: String::new(),
                ..Default::default()
            },
        );

        // Overwrite seq=1 (should not change insertion order)
        cache.insert(
            1,
            CachedLeaseResponse {
                ok: false,
                lease_id: Vec::new(),
                ttl_ms: 0,
                hint: "overwritten".to_string(),
                code: "TEST".to_string(),
                ..Default::default()
            },
        );

        // Updated value is returned
        let hit = cache.get(1).unwrap();
        assert!(!hit.ok);
        assert_eq!(hit.hint, "overwritten");
    }
}
