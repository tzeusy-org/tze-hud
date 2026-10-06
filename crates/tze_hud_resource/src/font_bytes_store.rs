//! In-memory store for raw font bytes indexed by `ResourceId`.
//!
//! Only `FONT_TTF` and `FONT_OTF` resources are stored here; image resources
//! are excluded because the compositor does not need their raw bytes after
//! decode validation.
//!
//! ## Design notes
//!
//! - Bytes are stored as `Arc<[u8]>` so multiple consumers (e.g., different
//!   compositor instances in tests) can hold a reference without copying.
//! - Uses `DashMap` for the same shard-concurrent properties as `DedupIndex`.
//! - `Clone`-able via the inner `Arc` — cloning the store shares ownership of
//!   the map (and therefore the byte arcs).  This matches `ResourceStore`.

use std::sync::Arc;

use dashmap::DashMap;

use crate::types::ResourceId;

// ─── FontBytesStore ──────────────────────────────────────────────────────────

/// Thread-safe store for raw font bytes, keyed by `ResourceId`.
///
/// Retains source bytes admitted by the upload path. Inspection is test-only.
#[derive(Clone, Debug)]
pub struct FontBytesStore {
    inner: Arc<DashMap<ResourceId, Arc<[u8]>>>,
    resident_ledger: Option<crate::ResidentLedger>,
}

impl FontBytesStore {
    /// Create a new, empty store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
            resident_ledger: None,
        }
    }

    pub fn new_with_resident_ledger(resident_ledger: crate::ResidentLedger) -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
            resident_ledger: Some(resident_ledger),
        }
    }

    pub fn try_insert(
        &self,
        resource_id: ResourceId,
        data: Arc<[u8]>,
    ) -> Result<bool, crate::ResidentReserveError> {
        use dashmap::mapref::entry::Entry;
        match self.inner.entry(resource_id) {
            Entry::Vacant(entry) => {
                if let Some(ledger) = &self.resident_ledger {
                    ledger.reserve(
                        crate::ResidentClass::Font,
                        format!("font:raw:{resource_id}"),
                        data.len() as u64,
                    )?;
                }
                entry.insert(data);
                Ok(true)
            }
            Entry::Occupied(_) => Ok(false),
        }
    }

    /// Insert raw font bytes for `resource_id`.
    ///
    /// If an entry already exists for this `ResourceId` (dedup race), the
    /// existing entry is kept and the new bytes are ignored — content-addressed
    /// identity guarantees the bytes are identical.
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert(&self, resource_id: ResourceId, data: Arc<[u8]>) {
        let _ = self.try_insert(resource_id, data);
    }

    /// Retrieve the raw bytes for `resource_id`.
    ///
    /// Returns `None` if not present (font not yet uploaded).
    #[cfg(any(test, feature = "test-support"))]
    #[inline]
    pub fn get(&self, resource_id: &ResourceId) -> Option<Arc<[u8]>> {
        self.inner.get(resource_id).map(|e| Arc::clone(e.value()))
    }

    /// Number of font entries currently held.
    #[cfg(any(test, feature = "test-support"))]
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` when no fonts are stored.
    #[cfg(any(test, feature = "test-support"))]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Default for FontBytesStore {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(n: u8) -> ResourceId {
        ResourceId::from_content(&[n])
    }

    #[test]
    fn empty_on_construction() {
        let store = FontBytesStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn insert_and_get_roundtrip() {
        let store = FontBytesStore::new();
        let id = rid(1);
        let data: Arc<[u8]> = Arc::from(b"fake-font-bytes".as_ref());
        store.insert(id, Arc::clone(&data));

        let retrieved = store.get(&id).expect("should be present");
        assert_eq!(&*retrieved, b"fake-font-bytes");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn dedup_race_keeps_existing_bytes() {
        let store = FontBytesStore::new();
        let id = rid(2);
        let first: Arc<[u8]> = Arc::from(b"first".as_ref());
        let second: Arc<[u8]> = Arc::from(b"second".as_ref());

        store.insert(id, Arc::clone(&first));
        store.insert(id, Arc::clone(&second)); // should be ignored

        let retrieved = store.get(&id).expect("should be present");
        assert_eq!(&*retrieved, b"first", "existing entry must not be replaced");
    }

    #[test]
    fn get_returns_none_for_missing_id() {
        let store = FontBytesStore::new();
        assert!(store.get(&rid(0xFF)).is_none());
    }

    #[test]
    fn clone_shares_state() {
        let store = FontBytesStore::new();
        let id = rid(4);
        store.insert(id, Arc::from(b"shared".as_ref()));

        // Cloning shares the inner Arc<DashMap>.
        let clone = store.clone();
        assert!(
            clone.get(&id).is_some(),
            "clone must see entries from original"
        );

        // Insert via clone — original must also see it.
        let id2 = rid(5);
        clone.insert(id2, Arc::from(b"via-clone".as_ref()));
        assert!(
            store.get(&id2).is_some(),
            "original must see entries from clone"
        );
    }

    #[test]
    fn font_source_admission_uses_font_class_and_is_atomic() {
        let ledger = crate::ResidentLedger::new(crate::ResidentLedgerLimits {
            aggregate_bytes: 4,
            resource_bytes: 0,
            widget_source_bytes: 0,
            widget_raster_bytes: 0,
            font_bytes: 4,
        });
        let store = FontBytesStore::new_with_resident_ledger(ledger.clone());
        let id = rid(9);

        assert!(store.try_insert(id, Arc::from(b"12345".as_ref())).is_err());
        assert!(store.get(&id).is_none());
        assert_eq!(ledger.snapshot().aggregate_bytes, 0);
        assert_eq!(ledger.snapshot().font_bytes, 0);
    }
}
