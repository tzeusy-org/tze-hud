//! Per-session MutationBatch deduplication window (RFC 0005 §5.2).
//!
//! The runtime maintains a per-session deduplication window keyed on `batch_id`
//! (16-byte UUIDv7 encoded as `Vec<u8>`). On a duplicate `batch_id` within the
//! window the runtime returns the cached `MutationResult` without re-applying
//! mutations.
//!
//! ## Window expiry
//!
//! A window entry expires under either of two conditions (whichever comes first):
//! - The per-session entry count reaches `max_entries` (default: 1000). On
//!   overflow the oldest entry is evicted (FIFO).
//! - The entry age exceeds `ttl_s` (default: 60 seconds). Stale entries are
//!   purged lazily on the next `insert` or `lookup`.
//!
//! ## Cloning
//!
//! `CachedResult` intentionally derives `Clone` so that the caller can cheaply
//! return a copy of the cached result while keeping the original in the window.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// A cached outcome for a previously-processed `MutationBatch`.
#[derive(Debug, Clone)]
pub struct CachedResult {
    /// Whether the batch was accepted by the scene graph.
    pub accepted: bool,
    /// SceneIds of scene objects created by the batch.
    pub created_ids: Vec<Vec<u8>>,
    /// Error code if `accepted` is false.
    pub error_code: String,
    /// Error message if `accepted` is false.
    pub error_message: String,
}

/// Entry stored in the deduplication table.
struct Entry {
    result: CachedResult,
    inserted_at: Instant,
}

/// Per-session deduplication window.
///
/// Thread-safety: this type performs no internal synchronization and is not
/// safe for concurrent mutation without external coordination (e.g., by
/// holding the per-session `Mutex`). Note: the underlying types (`HashMap`,
/// `VecDeque`, `Instant`) are `Send + Sync`; the constraint is about mutation
/// safety, not trait bounds.
pub struct DedupWindow {
    /// Maximum number of unique batch IDs stored before FIFO eviction.
    max_entries: usize,
    /// Maximum age of an entry before it is considered expired.
    ttl: Duration,
    /// Insertion-ordered queue of batch IDs for FIFO eviction.
    order: VecDeque<Vec<u8>>,
    /// Map from batch_id bytes to the cached result.
    cache: HashMap<Vec<u8>, Entry>,
}

impl DedupWindow {
    /// Create a new deduplication window.
    ///
    /// - `max_entries`: maximum unique batch IDs before FIFO eviction (spec default: 1000).
    /// - `ttl_s`: maximum age in seconds before expiry (spec default: 60).
    pub fn new(max_entries: usize, ttl_s: u64) -> Self {
        Self {
            max_entries,
            ttl: Duration::from_secs(ttl_s),
            order: VecDeque::with_capacity(max_entries),
            cache: HashMap::with_capacity(max_entries),
        }
    }

    /// Purge entries that have exceeded the TTL.
    ///
    /// Called lazily before every `insert` or `lookup` so the window never
    /// silently holds stale entries beyond the spec-mandated 60-second window.
    fn purge_expired(&mut self) {
        let now = Instant::now();
        while let Some(front) = self.order.front() {
            match self.cache.get(front) {
                Some(entry) if now.duration_since(entry.inserted_at) >= self.ttl => {
                    let key = self.order.pop_front().unwrap();
                    self.cache.remove(&key);
                }
                _ => break,
            }
        }
    }

    /// Look up a `batch_id` in the window.
    ///
    /// Returns `Some(CachedResult)` if the ID is present and not yet expired,
    /// `None` otherwise.
    ///
    /// Expired entries are purged lazily before the lookup.
    pub fn lookup(&mut self, batch_id: &[u8]) -> Option<CachedResult> {
        self.purge_expired();
        let now = Instant::now();
        match self.cache.get(batch_id) {
            Some(entry) if now.duration_since(entry.inserted_at) < self.ttl => {
                Some(entry.result.clone())
            }
            Some(_) => {
                // Individual entry expired but not yet swept (edge case under heavy load).
                // Treat as a cache miss; the entry will be replaced by `insert`.
                None
            }
            None => None,
        }
    }

    /// Insert a new `batch_id` → `CachedResult` mapping.
    ///
    /// If the window is at capacity, the oldest entry is evicted before insertion
    /// (FIFO).  Expired entries are purged before the capacity check.
    ///
    /// Re-inserting an existing `batch_id` (possible after TTL expiry and re-use)
    /// updates the entry in place and moves it to the back of the FIFO queue.
    pub fn insert(&mut self, batch_id: Vec<u8>, result: CachedResult) {
        self.purge_expired();

        // If the key already exists, remove it from the cache and order queue
        // so we can re-insert at the back with a fresh timestamp.
        if self.cache.remove(&batch_id).is_some() {
            self.order.retain(|k| k != &batch_id);
        }

        // Evict oldest entry if at capacity.
        if self.cache.len() >= self.max_entries {
            if let Some(oldest) = self.order.pop_front() {
                self.cache.remove(&oldest);
            }
        }

        self.order.push_back(batch_id.clone());
        self.cache.insert(
            batch_id,
            Entry {
                result,
                inserted_at: Instant::now(),
            },
        );
    }

    /// Return the number of entries currently in the window (including potentially
    /// expired ones that have not yet been purged).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.cache.len()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(accepted: bool) -> CachedResult {
        CachedResult {
            accepted,
            created_ids: Vec::new(),
            error_code: if accepted {
                String::new()
            } else {
                "ERR".to_string()
            },
            error_message: String::new(),
        }
    }

    #[test]
    fn test_basic_insert_lookup() {
        let mut w = DedupWindow::new(1000, 60);
        let id = b"batch-id-0000000".to_vec();
        assert!(w.lookup(&id).is_none());

        w.insert(id.clone(), make_result(true));
        let cached = w.lookup(&id).expect("should hit cache");
        assert!(cached.accepted);
    }

    #[test]
    fn test_different_ids_do_not_collide() {
        let mut w = DedupWindow::new(1000, 60);
        let id1 = b"aaaaaaaaaaaaaaaa".to_vec();
        let id2 = b"bbbbbbbbbbbbbbbb".to_vec();
        w.insert(id1.clone(), make_result(true));
        w.insert(id2.clone(), make_result(false));
        assert!(w.lookup(&id1).unwrap().accepted);
        assert!(!w.lookup(&id2).unwrap().accepted);
    }

    #[test]
    fn test_fifo_eviction_on_capacity() {
        let mut w = DedupWindow::new(3, 60);
        let ids: Vec<Vec<u8>> = (0u8..5).map(|i| vec![i; 16]).collect();

        for id in &ids[..3] {
            w.insert(id.clone(), make_result(true));
        }
        assert_eq!(w.len(), 3);

        // Insert 4th → oldest (ids[0]) evicted
        w.insert(ids[3].clone(), make_result(true));
        assert!(w.lookup(&ids[0]).is_none(), "oldest should be evicted");
        assert!(w.lookup(&ids[1]).is_some());
        assert!(w.lookup(&ids[2]).is_some());
        assert!(w.lookup(&ids[3]).is_some());

        // Insert 5th → ids[1] evicted
        w.insert(ids[4].clone(), make_result(true));
        assert!(
            w.lookup(&ids[1]).is_none(),
            "second oldest should be evicted"
        );
        assert!(w.lookup(&ids[2]).is_some());
        assert!(w.lookup(&ids[3]).is_some());
        assert!(w.lookup(&ids[4]).is_some());
    }

    #[test]
    fn test_ttl_expiry() {
        // Use a 1-second TTL for testability; we'll manually manipulate Instants
        // by inserting and then waiting.  For unit test speed we use a 0-second
        // TTL which expires immediately.
        let mut w = DedupWindow::new(1000, 0); // 0s = expires immediately
        let id = b"expiring00000000".to_vec();
        w.insert(id.clone(), make_result(true));
        // With ttl=0, the entry should be absent after the next lookup.
        assert!(
            w.lookup(&id).is_none(),
            "entry should have expired immediately"
        );
    }

    #[test]
    fn test_re_insert_after_expiry_treated_as_new() {
        let mut w = DedupWindow::new(1000, 0); // expires immediately
        let id = b"reinsert00000000".to_vec();
        w.insert(id.clone(), make_result(false));
        // With ttl=0, entry expires immediately.
        assert!(w.lookup(&id).is_none(), "ttl=0 entry should miss on lookup");

        // Re-insert as new with different result.
        w.insert(id.clone(), make_result(true));
        // The freshly inserted entry is present in the cache (not yet purged).
        // A lookup triggers purge_expired, causing this entry to be swept too.
        assert_eq!(w.len(), 1, "freshly inserted entry exists before lookup");
        // Lookup triggers purge; with ttl=0 the new entry also expires immediately.
        assert!(
            w.lookup(&id).is_none(),
            "ttl=0 re-insert should also miss on lookup"
        );
        // After lookup purge, cache is empty.
        assert_eq!(w.len(), 0, "entry purged after ttl=0 lookup");
    }

    #[test]
    fn test_created_ids_preserved() {
        let mut w = DedupWindow::new(1000, 60);
        let id = b"created-ids00000".to_vec();
        let result = CachedResult {
            accepted: true,
            created_ids: vec![vec![1u8; 16], vec![2u8; 16]],
            error_code: String::new(),
            error_message: String::new(),
        };
        w.insert(id.clone(), result);
        let cached = w.lookup(&id).unwrap();
        assert_eq!(cached.created_ids.len(), 2);
        assert_eq!(cached.created_ids[0], vec![1u8; 16]);
        assert_eq!(cached.created_ids[1], vec![2u8; 16]);
    }
}
