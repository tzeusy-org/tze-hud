//! Element-store persistence: keep the on-disk element registry in step with
//! tiles, zones, and widgets as sessions create and publish to them.

use crate::session::{ElementStoreFile, SharedState};
use tze_hud_scene::SceneId;
use tze_hud_scene::element_store::{ElementStore, ElementType};

/// Captures the data needed to persist the element store outside the shared-state lock.
pub(super) struct ElementStorePersistRequest {
    pub(super) store: ElementStore,
    pub(super) file: ElementStoreFile,
}

/// Persist the element store without blocking the async executor worker thread.
pub(super) async fn persist_element_store(request: Option<ElementStorePersistRequest>) {
    let Some(request) = request else {
        return;
    };

    let path_for_log = request.file.path.clone();
    match tokio::task::spawn_blocking(move || request.file.save(&request.store)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!(
                path = %path_for_log.display(),
                error = %err,
                "element_store: failed to persist tile IDs"
            );
        }
        Err(err) => {
            tracing::warn!(
                path = %path_for_log.display(),
                error = %err,
                "element_store: failed to join tile ID persistence task"
            );
        }
    }
}

fn touch_element_store_entry_by_id(
    st: &mut SharedState,
    element_id: SceneId,
    element_type: ElementType,
    now: u64,
) -> Option<ElementStorePersistRequest> {
    let entry = st.element_store.entries.get_mut(&element_id)?;
    if entry.element_type != element_type {
        return None;
    }
    entry.last_published_at = now;
    st.element_store_file
        .clone()
        .map(|file| ElementStorePersistRequest {
            store: st.element_store.clone(),
            file,
        })
}

pub(super) fn touch_element_store_entry_by_namespace(
    st: &mut SharedState,
    element_type: ElementType,
    namespace: &str,
    now: u64,
) -> Option<ElementStorePersistRequest> {
    let id = st
        .element_store
        .find_id_by_type_namespace(element_type, namespace)?;
    touch_element_store_entry_by_id(st, id, element_type, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static SAVED: Mutex<Vec<(std::path::PathBuf, usize)>> = Mutex::new(Vec::new());

    fn record(store: &ElementStore, path: &std::path::Path) -> std::io::Result<()> {
        SAVED
            .lock()
            .unwrap()
            .push((path.to_path_buf(), store.entries.len()));
        Ok(())
    }

    /// A session-side save goes through the writer the runtime handed in,
    /// with the snapshot taken under the lock.
    #[tokio::test]
    async fn persist_uses_the_injected_writer() {
        let path = std::path::PathBuf::from("element_store.toml");
        persist_element_store(Some(ElementStorePersistRequest {
            store: ElementStore::default(),
            file: ElementStoreFile {
                path: path.clone(),
                write: record,
            },
        }))
        .await;
        persist_element_store(None).await;
        assert_eq!(*SAVED.lock().unwrap(), [(path, 0)]);
    }
}
