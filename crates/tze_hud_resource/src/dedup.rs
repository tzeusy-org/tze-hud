//! Content-addressed deduplication index.
//!
//! The dedup index maps `ResourceId` → `ResourceRecord`.  It is the single
//! source of truth for whether a resource already exists in the store.
//!
//! # Performance contract
//!
//! Dedup lookups MUST complete within 100 μs (spec line 103, lines 112-113).
//! The implementation uses `DashMap` — a sharded concurrent hash map —
//! so read operations are lock-free for non-contended shards and scale with
//! CPU count.  On modern hardware a `DashMap::contains_key` on a warm cache
//! takes O(1) ns, well within the 100 μs budget.
//!
//! # Immutability guarantee
//!
//! Once inserted, a `ResourceRecord` is never mutated.  The content at a
//! given `ResourceId` is immutable for the lifetime of the store
//! (spec lines 20-24).

use std::sync::Arc;

use dashmap::DashMap;

use crate::types::{DecodedMeta, ResourceId, ResourceType};

// ─── Resource record ─────────────────────────────────────────────────────────

/// A stored resource entry in the dedup index.
#[derive(Debug)]
pub struct ResourceRecord {
    /// Content-addressed identifier (equals the map key; stored for
    /// convenience).
    pub resource_id: ResourceId,
    /// V1 resource type.
    pub resource_type: ResourceType,
    /// Decoded in-memory size (bytes).  Used for budget accounting.
    pub decoded_bytes: usize,
}

impl ResourceRecord {
    pub fn new(resource_id: ResourceId, resource_type: ResourceType, meta: &DecodedMeta) -> Self {
        Self {
            resource_id,
            resource_type,
            decoded_bytes: meta.decoded_bytes,
        }
    }
}

// ─── Dedup index ─────────────────────────────────────────────────────────────

/// Sharded concurrent dedup index.
///
/// Designed to be wrapped in an `Arc` and shared across all tasks that need
/// to check for or insert resources.
#[derive(Clone, Debug)]
pub struct DedupIndex {
    /// `ResourceId` → `Arc<ResourceRecord>`.
    ///
    /// `Arc` indirection allows callers to hold a reference to the record
    /// without holding the shard lock.  Insertion is append-only; the record
    /// itself is never replaced (immutability guarantee).
    inner: Arc<DashMap<ResourceId, Arc<ResourceRecord>>>,
}

impl DedupIndex {
    /// Create an empty dedup index.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    /// Check whether `resource_id` is already in the store.
    ///
    /// Latency contract: MUST complete within 100 μs (spec lines 112-113).
    /// DashMap read is O(1) and lock-free for non-contended shards.
    #[cfg(any(test, feature = "test-support"))]
    #[inline]
    pub fn contains(&self, resource_id: &ResourceId) -> bool {
        self.inner.contains_key(resource_id)
    }

    /// Look up a record by `ResourceId`.  Returns `None` if not present.
    ///
    /// The caller receives a cloned `Arc` — the shard lock is not held after
    /// this call returns.
    #[inline]
    pub fn get(&self, resource_id: &ResourceId) -> Option<Arc<ResourceRecord>> {
        self.inner
            .get(resource_id)
            .map(|entry| Arc::clone(entry.value()))
    }

    /// Insert a new record.  Returns `Err` if a record with the same id
    /// already exists (immutability: never replace an existing entry).
    ///
    /// The upload path first looks up the record; a collision can still occur
    /// when another upload inserts the same content before this call.
    pub fn insert(
        &self,
        resource_id: ResourceId,
        record: ResourceRecord,
    ) -> Result<Arc<ResourceRecord>, Arc<ResourceRecord>> {
        let arc = Arc::new(record);
        // DashMap::entry provides shard-level locking so insert is atomic
        // from the perspective of other callers on the same key.
        use dashmap::mapref::entry::Entry;
        match self.inner.entry(resource_id) {
            Entry::Vacant(e) => {
                let inserted = Arc::clone(&arc);
                e.insert(arc);
                Ok(inserted)
            }
            Entry::Occupied(e) => {
                // Already exists — return the existing record.
                Err(Arc::clone(e.get()))
            }
        }
    }

    /// Current number of resources in the store.
    #[expect(
        clippy::len_without_is_empty,
        reason = "Resource admission needs the count; no caller needs an emptiness API."
    )]
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Total decoded bytes across all stored resources.
    ///
    /// Used for the runtime-wide texture budget check.  Iterates all shards;
    /// not intended for hot-path use.
    pub fn total_decoded_bytes(&self) -> usize {
        self.inner
            .iter()
            .map(|entry| entry.value().decoded_bytes)
            .sum()
    }
}

impl Default for DedupIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DecodedMeta, ResourceType};

    fn make_record(id: ResourceId, decoded_bytes: usize) -> ResourceRecord {
        ResourceRecord::new(
            id,
            ResourceType::ImagePng,
            &DecodedMeta {
                decoded_bytes,
                width_px: 64,
                height_px: 64,
            },
        )
    }

    #[test]
    fn insert_and_contains() {
        let index = DedupIndex::new();
        let id = ResourceId::from_content(b"test-resource");
        assert!(!index.contains(&id));

        index.insert(id, make_record(id, 1024)).unwrap();
        assert!(index.contains(&id));
    }

    #[test]
    fn get_returns_record() {
        let index = DedupIndex::new();
        let id = ResourceId::from_content(b"get-test");
        index.insert(id, make_record(id, 512)).unwrap();

        let rec = index.get(&id).expect("record should exist");
        assert_eq!(rec.resource_id, id);
        assert_eq!(rec.decoded_bytes, 512);
    }

    #[test]
    fn insert_duplicate_returns_existing() {
        let index = DedupIndex::new();
        let id = ResourceId::from_content(b"dup");

        index.insert(id, make_record(id, 100)).unwrap();
        let existing = index.insert(id, make_record(id, 200)).unwrap_err();
        // The original record (100 bytes) must be returned.
        assert_eq!(existing.decoded_bytes, 100);
    }

    #[test]
    fn total_decoded_bytes_sums_all_records() {
        let index = DedupIndex::new();
        for (i, size) in [(b"a" as &[u8], 100usize), (b"b", 200), (b"c", 300)] {
            let id = ResourceId::from_content(i);
            index.insert(id, make_record(id, size)).unwrap();
        }
        assert_eq!(index.total_decoded_bytes(), 600);
    }

    #[test]
    fn dedup_index_len() {
        let index = DedupIndex::new();
        assert_eq!(index.len(), 0);
        let id = ResourceId::from_content(b"len-test");
        index.insert(id, make_record(id, 64)).unwrap();
        assert_eq!(index.len(), 1);
    }
}
