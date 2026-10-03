//! Runtime bootstrap, reconciliation, and TOML persistence for element IDs.
//!
//! This module owns all I/O for the element-store: TOML serialization, atomic
//! file writes, and platform-specific file-replace helpers.  The pure data
//! model lives in `tze_hud_scene::element_store`; the bootstrap/reconcile
//! logic and all disk operations live here.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tze_hud_scene::element_store::{
    ElementStore, ElementStoreEntry, ElementType, MAX_UNSEEN_ORPHAN_OVERRIDE_RESTARTS,
};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::SceneId;

// ── TOML serialization ────────────────────────────────────────────────────────

/// Parse a TOML-encoded element store.
pub fn from_toml_str(input: &str) -> Result<ElementStore, toml::de::Error> {
    toml::from_str(input)
}

/// Serialize the store to TOML.
pub fn to_toml_string(store: &ElementStore) -> Result<String, toml::ser::Error> {
    toml::to_string_pretty(store)
}

// ── File I/O ──────────────────────────────────────────────────────────────────

/// Load an element store from disk.
///
/// Missing files are treated as first boot and return an empty store.
pub fn load_element_store(path: &Path) -> io::Result<ElementStore> {
    match fs::read_to_string(path) {
        Ok(content) => from_toml_str(&content).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid element_store TOML: {err}"),
            )
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(ElementStore::default()),
        Err(err) => Err(err),
    }
}

/// Persist an element store atomically using write-to-temp + replace.
pub fn persist_element_store_to_path(store: &ElementStore, path: &Path) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let temp_path = unique_temp_path(path);
    let toml_text = to_toml_string(store).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to serialize element_store TOML: {err}"),
        )
    })?;

    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)?;
    file.write_all(toml_text.as_bytes())?;
    file.sync_all()?;
    drop(file);

    if let Err(err) = replace_file_atomically(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    sync_parent_dir(parent)?;

    Ok(())
}

fn unique_temp_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("element_store.toml");
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!(
        ".{stem}.tmp.{}.{}.{}",
        std::process::id(),
        now_ns,
        SceneId::new()
    ))
}

#[cfg(not(target_os = "windows"))]
fn replace_file_atomically(src: &Path, dst: &Path) -> io::Result<()> {
    fs::rename(src, dst)
}

#[cfg(target_os = "windows")]
fn replace_file_atomically(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    fn to_wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let src_w = to_wide(src);
    let dst_w = to_wide(dst);

    // SAFETY: pointers are valid NUL-terminated UTF-16 buffers for the duration
    // of the call and reference local immutable vectors.
    unsafe {
        MoveFileExW(
            PCWSTR(src_w.as_ptr()),
            PCWSTR(dst_w.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(|err| io::Error::other(format!("{err}")))?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn sync_parent_dir(path: &Path) -> io::Result<()> {
    OpenOptions::new().read(true).open(path)?.sync_all()
}

#[cfg(target_os = "windows")]
fn sync_parent_dir(_path: &Path) -> io::Result<()> {
    // Windows path replacement uses MoveFileExW with MOVEFILE_WRITE_THROUGH.
    Ok(())
}

// ── Bootstrap ─────────────────────────────────────────────────────────────────

/// Result of element-store startup bootstrap.
#[derive(Clone, Debug)]
pub struct ElementStoreBootstrap {
    pub path: PathBuf,
    pub store: ElementStore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StorePlatform {
    Windows,
    Linux,
    MacOs,
}

/// Resolve the runtime path for `element_store.toml`.
///
/// Environment override:
/// - `TZE_HUD_ELEMENT_STORE_PATH`
pub fn resolve_element_store_path() -> PathBuf {
    if let Some(override_path) = env_var("TZE_HUD_ELEMENT_STORE_PATH") {
        return PathBuf::from(override_path);
    }

    let platform = if cfg!(target_os = "windows") {
        StorePlatform::Windows
    } else if cfg!(target_os = "macos") {
        StorePlatform::MacOs
    } else {
        StorePlatform::Linux
    };

    resolve_element_store_path_for(platform, &|key| env_var(key))
}

/// Load, reconcile, and persist the element store for the given scene.
pub fn bootstrap_scene_element_store(scene: &mut SceneGraph) -> ElementStoreBootstrap {
    bootstrap_scene_element_store_with_path(scene, resolve_element_store_path())
}

/// Same as [`bootstrap_scene_element_store`], but with an explicit path.
pub fn bootstrap_scene_element_store_with_path(
    scene: &mut SceneGraph,
    path: PathBuf,
) -> ElementStoreBootstrap {
    let existed = path.exists();
    let mut store = match load_element_store(&path) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "element_store: failed to load store, continuing with empty store"
            );
            ElementStore::default()
        }
    };

    let now_ms = now_wall_ms();
    let changed = reconcile_scene_ids(scene, &mut store, now_ms);
    // Bounded retention for orphan tile entries (hud-fwgv7 override-bearing,
    // hud-m1ok3 override-free). Runs before tiles are recreated (they are
    // republished after bootstrap), so every tile entry is an orphan here;
    // `adopt_orphaned_tile_overrides` later reclaims override-bearing ones whose
    // portals republish, while override-free entries for republishing portals are
    // superseded by fresh-id entries. Entries whose portal was closed permanently
    // accumulate unseen restarts and are pruned.
    let gc = store.gc_expired_orphan_tile_overrides(MAX_UNSEEN_ORPHAN_OVERRIDE_RESTARTS);
    if !gc.pruned.is_empty() {
        tracing::info!(
            pruned = gc.pruned.len(),
            "element_store: pruned orphan tile entries past retention bound (hud-fwgv7, hud-m1ok3)"
        );
    }
    relock_tiles_with_durable_override(scene, &store);
    if changed || gc.mutated || !existed {
        if let Err(err) = persist_element_store_to_path(&store, &path) {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "element_store: failed to persist reconciled store"
            );
        }
    }

    ElementStoreBootstrap { path, store }
}

/// Re-establish viewer geometry authority for every tile that loaded with a
/// durable user geometry override (hud-8vejp).
///
/// The `viewer_geometry_locked` set is ephemeral (serde-skip), so a runtime
/// restart drops it. Without re-locking, an adapter `UpdateTileBounds`
/// republish after restart could reposition a portal member whose whole-group
/// resize/move the viewer had already committed — fracturing the group until
/// the next viewer gesture re-took the lock. The durable per-member override in
/// the element store is the source of truth for "the viewer owns this member's
/// geometry", so any tile entry carrying one is re-locked at startup.
///
/// Locking a scene id that has no live tile yet is harmless: the lock is a set
/// membership check consulted by `SceneGraph::update_tile_bounds`, and if a
/// tile with that id is later reconstructed the authority is already in force.
fn relock_tiles_with_durable_override(scene: &mut SceneGraph, store: &ElementStore) {
    for (id, entry) in &store.entries {
        if entry.element_type == ElementType::Tile && entry.geometry_override.is_some() {
            scene.lock_viewer_geometry(*id);
        }
    }
}

/// Reconcile persisted IDs with startup-registered zones and widgets.
///
/// Matching keys:
/// - zone: `zone.name`
/// - widget: `instance.instance_name`
pub fn reconcile_scene_ids(scene: &mut SceneGraph, store: &mut ElementStore, now_ms: u64) -> bool {
    let mut changed = false;

    for zone in scene.zone_registry.zones.values_mut() {
        let resolved_id = match store.find_id_by_type_namespace(ElementType::Zone, &zone.name) {
            Some(id) => id,
            None => {
                if zone.id.is_null() {
                    zone.id = SceneId::new();
                    changed = true;
                }
                zone.id
            }
        };

        if zone.id != resolved_id {
            zone.id = resolved_id;
            changed = true;
        }

        if ensure_entry(
            store,
            resolved_id,
            ElementType::Zone,
            zone.name.clone(),
            now_ms,
        ) {
            changed = true;
        }
    }

    for instance in scene.widget_registry.instances.values_mut() {
        let resolved_id =
            match store.find_id_by_type_namespace(ElementType::Widget, &instance.instance_name) {
                Some(id) => id,
                None => {
                    if instance.id.is_null() {
                        instance.id = SceneId::new();
                        changed = true;
                    }
                    instance.id
                }
            };

        if instance.id != resolved_id {
            instance.id = resolved_id;
            changed = true;
        }

        if ensure_entry(
            store,
            resolved_id,
            ElementType::Widget,
            instance.instance_name.clone(),
            now_ms,
        ) {
            changed = true;
        }
    }

    changed
}

fn ensure_entry(
    store: &mut ElementStore,
    id: SceneId,
    element_type: ElementType,
    namespace: String,
    now_ms: u64,
) -> bool {
    match store.entries.get_mut(&id) {
        Some(entry) => {
            let mut changed = false;
            if entry.element_type != element_type {
                entry.element_type = element_type;
                changed = true;
            }
            if entry.namespace != namespace {
                entry.namespace = namespace;
                changed = true;
            }
            if entry.created_at == 0 {
                entry.created_at = now_ms;
                changed = true;
            }
            if entry.last_published_at == 0 {
                entry.last_published_at = now_ms;
                changed = true;
            }
            changed
        }
        None => {
            store.entries.insert(
                id,
                ElementStoreEntry {
                    element_type,
                    namespace,
                    created_at: now_ms,
                    last_published_at: now_ms,
                    z_order: 0,
                    unseen_restarts: 0,
                    geometry_override: None,
                },
            );
            true
        }
    }
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn now_wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn resolve_element_store_path_for<F>(platform: StorePlatform, env_lookup: &F) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    let dir = match platform {
        StorePlatform::Windows => env_lookup("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("tze_hud"),
        StorePlatform::MacOs => env_lookup("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("Library")
            .join("Application Support")
            .join("tze_hud"),
        StorePlatform::Linux => {
            if let Some(xdg_data_home) = env_lookup("XDG_DATA_HOME") {
                PathBuf::from(xdg_data_home).join("tze_hud")
            } else if let Some(home) = env_lookup("HOME") {
                PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("tze_hud")
            } else {
                std::env::temp_dir().join("tze_hud")
            }
        }
    };
    dir.join("element_store.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;
    use tze_hud_scene::types::{
        ContentionPolicy, GeometryPolicy, RenderingPolicy, WidgetInstance, WidgetParameterValue,
        ZoneDefinition, ZoneMediaType,
    };

    fn make_scene_with_zone_and_widget() -> SceneGraph {
        let mut scene = SceneGraph::new(1920.0, 1080.0);

        let tab_id = scene.create_tab("main", 0).expect("tab");
        scene.zone_registry.register(ZoneDefinition {
            id: SceneId::new(),
            name: "subtitle".to_string(),
            description: "subtitle zone".to_string(),
            geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.9,
                width_pct: 1.0,
                height_pct: 0.1,
            },
            accepted_media_types: vec![ZoneMediaType::StreamText],
            rendering_policy: RenderingPolicy::default(),
            contention_policy: ContentionPolicy::LatestWins,
            max_publishers: 4,
            auto_clear_ms: None,
            ephemeral: false,
            layer_attachment: Default::default(),
        });

        scene.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "gauge".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: "gauge-main".to_string(),
            current_params: HashMap::<String, WidgetParameterValue>::new(),
        });

        scene
    }

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tze_hud_runtime_element_store_{name}_{}",
            SceneId::new()
        ))
    }

    // ── TOML and I/O tests ────────────────────────────────────────────────

    fn sample_store() -> ElementStore {
        use tze_hud_scene::element_store::ElementStoreEntry;
        use tze_hud_scene::types::GeometryPolicy;

        let now = 1_710_000_000_000u64;
        let mut entries = std::collections::HashMap::new();
        entries.insert(
            SceneId::new(),
            ElementStoreEntry {
                element_type: ElementType::Zone,
                namespace: "subtitle".to_string(),
                created_at: now,
                last_published_at: now,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: None,
            },
        );
        entries.insert(
            SceneId::new(),
            ElementStoreEntry {
                element_type: ElementType::Widget,
                namespace: "gauge-main".to_string(),
                created_at: now + 1,
                last_published_at: now + 1,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: Some(GeometryPolicy::Relative {
                    x_pct: 0.1,
                    y_pct: 0.2,
                    width_pct: 0.3,
                    height_pct: 0.4,
                }),
            },
        );
        ElementStore { entries }
    }

    #[test]
    fn toml_round_trip_preserves_entries() {
        let store = sample_store();
        let text = to_toml_string(&store).expect("serialize");
        let restored = from_toml_str(&text).expect("deserialize");
        assert_eq!(restored, store);
    }

    #[test]
    fn missing_file_loads_empty_store() {
        let path = test_path("missing-load");
        let _ = std::fs::remove_file(&path);
        let store = load_element_store(&path).expect("load default");
        assert!(store.entries.is_empty());
    }

    #[test]
    fn persist_atomic_replaces_existing_file() {
        use tze_hud_scene::element_store::ElementStoreEntry;

        let path = test_path("atomic-replace");
        let _ = std::fs::remove_file(&path);

        let first = sample_store();
        persist_element_store_to_path(&first, &path).expect("first persist");

        let mut second = ElementStore::default();
        second.entries.insert(
            SceneId::new(),
            ElementStoreEntry {
                element_type: ElementType::Tile,
                namespace: "agent.weather".to_string(),
                created_at: 7,
                last_published_at: 8,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: None,
            },
        );
        persist_element_store_to_path(&second, &path).expect("second persist");

        let restored = load_element_store(&path).expect("reload");
        assert_eq!(restored, second);

        let _ = std::fs::remove_file(&path);
    }

    // ── Path resolution tests ─────────────────────────────────────────────

    #[test]
    fn windows_path_uses_localappdata() {
        let path = resolve_element_store_path_for(StorePlatform::Windows, &|key| {
            (key == "LOCALAPPDATA").then_some("C:\\Users\\Alice\\AppData\\Local".to_string())
        });
        assert_eq!(
            path,
            Path::new("C:\\Users\\Alice\\AppData\\Local")
                .join("tze_hud")
                .join("element_store.toml")
        );
    }

    #[test]
    fn linux_path_prefers_xdg_data_home() {
        let path = resolve_element_store_path_for(StorePlatform::Linux, &|key| {
            (key == "XDG_DATA_HOME").then_some("/home/alice/.data".to_string())
        });
        assert_eq!(
            path,
            Path::new("/home/alice/.data")
                .join("tze_hud")
                .join("element_store.toml")
        );
    }

    #[test]
    fn linux_path_falls_back_to_home_local_share() {
        let path = resolve_element_store_path_for(StorePlatform::Linux, &|key| {
            (key == "HOME").then_some("/home/alice".to_string())
        });
        assert_eq!(
            path,
            Path::new("/home/alice")
                .join(".local")
                .join("share")
                .join("tze_hud")
                .join("element_store.toml")
        );
    }

    #[test]
    fn macos_path_uses_application_support() {
        let path = resolve_element_store_path_for(StorePlatform::MacOs, &|key| {
            (key == "HOME").then_some("/Users/alice".to_string())
        });
        assert_eq!(
            path,
            Path::new("/Users/alice")
                .join("Library")
                .join("Application Support")
                .join("tze_hud")
                .join("element_store.toml")
        );
    }

    #[test]
    fn reconcile_reuses_existing_zone_and_widget_ids() {
        let mut scene = make_scene_with_zone_and_widget();
        let desired_zone_id = SceneId::new();
        let desired_widget_id = SceneId::new();
        let mut store = ElementStore::default();
        store.entries.insert(
            desired_zone_id,
            ElementStoreEntry {
                element_type: ElementType::Zone,
                namespace: "subtitle".to_string(),
                created_at: 100,
                last_published_at: 200,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: None,
            },
        );
        store.entries.insert(
            desired_widget_id,
            ElementStoreEntry {
                element_type: ElementType::Widget,
                namespace: "gauge-main".to_string(),
                created_at: 100,
                last_published_at: 200,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: None,
            },
        );

        let changed = reconcile_scene_ids(&mut scene, &mut store, 300);
        assert!(changed, "scene IDs should be rewritten to persisted IDs");
        assert_eq!(scene.zone_registry.zones["subtitle"].id, desired_zone_id);
        assert_eq!(
            scene.widget_registry.instances["gauge-main"].id,
            desired_widget_id
        );
    }

    #[test]
    fn bootstrap_round_trip_reuses_ids_across_restart() {
        let path = test_path("restart");
        let _ = std::fs::remove_file(&path);

        let mut first_scene = make_scene_with_zone_and_widget();
        let first = bootstrap_scene_element_store_with_path(&mut first_scene, path.clone());
        assert!(
            !first.store.entries.is_empty(),
            "startup must persist zone/widget identities"
        );
        let first_zone_id = first_scene.zone_registry.zones["subtitle"].id;
        let first_widget_id = first_scene.widget_registry.instances["gauge-main"].id;

        let mut second_scene = make_scene_with_zone_and_widget();
        let _second = bootstrap_scene_element_store_with_path(&mut second_scene, path.clone());
        assert_eq!(
            second_scene.zone_registry.zones["subtitle"].id,
            first_zone_id
        );
        assert_eq!(
            second_scene.widget_registry.instances["gauge-main"].id,
            first_widget_id
        );

        let _ = std::fs::remove_file(path);
    }

    /// hud-8vejp: the `viewer_geometry_locked` set is serde-skip (dropped on
    /// restart), so bootstrap must re-establish the lock for every tile that
    /// loaded a durable geometry override — otherwise an adapter republish could
    /// reposition a portal member post-restart. A tile without an override, and
    /// a non-Tile entry with one, must not be locked.
    #[test]
    fn bootstrap_relocks_tiles_that_load_a_durable_override() {
        use tze_hud_scene::Rect;

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("main", 0).expect("tab");
        let lease = scene.grant_lease("agent", 60_000);
        let locked_tile = scene
            .create_tile(tab_id, "agent", lease, Rect::new(0.0, 0.0, 100.0, 80.0), 1)
            .expect("locked tile");
        let unlocked_tile = scene
            .create_tile(
                tab_id,
                "agent",
                lease,
                Rect::new(200.0, 0.0, 100.0, 80.0),
                1,
            )
            .expect("unlocked tile");

        let override_policy = GeometryPolicy::Relative {
            x_pct: 0.1,
            y_pct: 0.1,
            width_pct: 0.2,
            height_pct: 0.2,
        };
        let mut store = ElementStore::default();
        store.entries.insert(
            locked_tile,
            ElementStoreEntry {
                element_type: ElementType::Tile,
                namespace: "portal".to_string(),
                created_at: 1,
                last_published_at: 1,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: Some(override_policy),
            },
        );
        store.entries.insert(
            unlocked_tile,
            ElementStoreEntry {
                element_type: ElementType::Tile,
                namespace: "portal".to_string(),
                created_at: 1,
                last_published_at: 1,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: None,
            },
        );
        // A non-Tile entry carrying an override must NOT re-lock (widgets/zones
        // resolve geometry through their registries, not the tile-bounds lock).
        store.entries.insert(
            SceneId::new(),
            ElementStoreEntry {
                element_type: ElementType::Widget,
                namespace: "gauge".to_string(),
                created_at: 1,
                last_published_at: 1,
                z_order: 0,
                unseen_restarts: 0,
                geometry_override: Some(override_policy),
            },
        );

        relock_tiles_with_durable_override(&mut scene, &store);

        assert!(
            scene.is_viewer_geometry_locked(locked_tile),
            "a tile loaded with a durable override must be re-locked at startup"
        );
        assert!(
            !scene.is_viewer_geometry_locked(unlocked_tile),
            "a tile with no override must not be locked"
        );
    }
}
