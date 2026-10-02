//! Element-store persistence: keep the on-disk element registry in step with
//! tiles, zones, and widgets as sessions create and publish to them.

use super::now_ms;
use crate::session::SharedState;
use tze_hud_scene::SceneId;
use tze_hud_scene::element_store::{ElementStore, ElementStoreEntry, ElementType};

/// Captures the data needed to persist the element store outside the shared-state lock.
pub(super) struct ElementStorePersistRequest {
    pub(super) store: ElementStore,
    pub(super) path: std::path::PathBuf,
}

/// Update tile entries in the element store and return an optional persistence request.
pub(super) async fn persist_created_tile_entries(
    st: &mut SharedState,
    created_ids: &[SceneId],
) -> Option<ElementStorePersistRequest> {
    if created_ids.is_empty() {
        return None;
    }

    // `(id, namespace, z_order)` for each just-created tile, plus the ids of
    // every tile currently live in the scene (needed to tell a recreated portal
    // member's orphaned entry from a still-live sibling's — hud-08nls).
    let (created_tiles, live_ids): (
        Vec<(SceneId, String, u32)>,
        std::collections::HashSet<SceneId>,
    ) = {
        let scene = st.scene.lock().await;
        let created = created_ids
            .iter()
            .filter_map(|id| {
                scene
                    .tiles
                    .get(id)
                    .map(|tile| (*id, tile.namespace.clone(), tile.z_order))
            })
            .collect();
        let live = scene.tiles.keys().copied().collect();
        (created, live)
    };

    if created_tiles.is_empty() {
        return None;
    }

    let now = now_ms();
    let mut changed = false;
    let recreated: Vec<tze_hud_scene::element_store::RecreatedTile> = created_tiles
        .iter()
        .map(
            |(id, namespace, z_order)| tze_hud_scene::element_store::RecreatedTile {
                id: *id,
                namespace: namespace.clone(),
                z_order: *z_order,
            },
        )
        .collect();
    for (id, namespace, z_order) in created_tiles {
        match st.element_store.entries.get_mut(&id) {
            Some(entry) => {
                if entry.element_type != ElementType::Tile {
                    entry.element_type = ElementType::Tile;
                    changed = true;
                }
                if entry.namespace != namespace {
                    entry.namespace = namespace;
                    changed = true;
                }
                if entry.z_order != z_order {
                    entry.z_order = z_order;
                    changed = true;
                }
                if entry.created_at == 0 {
                    entry.created_at = now;
                    changed = true;
                }
                if entry.last_published_at != now {
                    entry.last_published_at = now;
                    changed = true;
                }
                // A just-published tile is live, so it starts a fresh retention
                // window; clear any accumulated unseen-restart count (hud-fwgv7).
                if entry.unseen_restarts != 0 {
                    entry.unseen_restarts = 0;
                    changed = true;
                }
                if entry.geometry_override.is_some() {
                    entry.geometry_override = None;
                    changed = true;
                }
            }
            None => {
                st.element_store.entries.insert(
                    id,
                    ElementStoreEntry {
                        element_type: ElementType::Tile,
                        namespace,
                        created_at: now,
                        last_published_at: now,
                        z_order,
                        unseen_restarts: 0,
                        geometry_override: None,
                    },
                );
                changed = true;
            }
        }
    }

    // Re-home any durable override whose portal member tile was recreated with a
    // fresh SceneId (the entries were just inserted above with no override; a
    // matching orphan hands its override over here). Re-lock viewer geometry for
    // each adopter so a subsequent adapter `UpdateTileBounds` republish cannot
    // reposition it before the viewer touches it again (mirrors the bootstrap
    // re-lock in `tze_hud_runtime::element_store`).
    let adopted = st
        .element_store
        .adopt_orphaned_tile_overrides(&recreated, &live_ids);
    if !adopted.is_empty() {
        changed = true;
        let mut scene = st.scene.lock().await;
        for id in &adopted {
            scene.lock_viewer_geometry(*id);
        }
    }

    if !changed {
        return None;
    }

    st.element_store_path
        .clone()
        .map(|path| ElementStorePersistRequest {
            store: st.element_store.clone(),
            path,
        })
}

/// Serialize and atomically write an [`ElementStore`] to disk.
///
/// This is the protocol-layer counterpart of
/// `tze_hud_runtime::element_store::persist_element_store_to_path`.  It is
/// intentionally a local copy so that `tze_hud_protocol` does not need to
/// depend on `tze_hud_runtime` (which would create a circular dependency since
/// `tze_hud_runtime` already depends on `tze_hud_protocol`).
fn write_element_store_to_path(
    store: &ElementStore,
    path: &std::path::Path,
) -> std::io::Result<()> {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    let toml_text = toml::to_string_pretty(store).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to serialize element_store TOML: {err}"),
        )
    })?;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("element_store.toml");
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_path = parent.join(format!(
        ".{stem}.tmp.{}.{}.{}",
        std::process::id(),
        now_ns,
        tze_hud_scene::types::SceneId::new()
    ));

    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)?;
    file.write_all(toml_text.as_bytes())?;
    file.sync_all()?;
    drop(file);

    if let Err(err) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    // On Unix, sync the parent directory so the rename is durable.
    // On Windows, the rename itself is sufficient.
    #[cfg(not(target_os = "windows"))]
    {
        OpenOptions::new().read(true).open(parent)?.sync_all()?;
    }

    Ok(())
}

/// Persist the element store without blocking the async executor worker thread.
pub(super) async fn persist_element_store(request: Option<ElementStorePersistRequest>) {
    let Some(request) = request else {
        return;
    };

    let path_for_log = request.path.clone();
    match tokio::task::spawn_blocking(move || {
        write_element_store_to_path(&request.store, &request.path)
    })
    .await
    {
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

pub(super) fn touch_element_store_entry_by_id(
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
    st.element_store_path
        .clone()
        .map(|path| ElementStorePersistRequest {
            store: st.element_store.clone(),
            path,
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
