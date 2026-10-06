//! Startup index for durable runtime widget SVG assets.
//!
//! Uploaded scene resources remain ephemeral; runtime widget SVG assets are
//! the durability exception. Startup preserves the existing blob/sidecar format:
//! - content-addressed deduplication by BLAKE3 hash
//! - startup re-index/reconcile (hash verification + sidecar validation)
//! - durable footprint budgets (global + per-agent)
//!
//! Test-only writers create persisted fixtures for the startup checks.

use std::collections::HashMap;
use std::fs;
#[cfg(test)]
use std::fs::{File, OpenOptions};
#[cfg(test)]
use std::io::Write;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::ResourceId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeWidgetStoreConfig {
    pub store_path: PathBuf,
    /// `0` means unbounded.
    pub max_total_bytes: u64,
    /// `0` means unbounded.
    pub max_agent_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimeWidgetAssetRecord {
    size_bytes: u64,
    agent_namespace: String,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum PutOutcome {
    Stored { resource_id: ResourceId },
    Deduplicated { resource_id: ResourceId },
}

#[derive(Debug, Error)]
pub enum RuntimeWidgetStoreError {
    #[error(
        "invalid runtime widget store config: max_agent_bytes ({max_agent_bytes}) exceeds max_total_bytes ({max_total_bytes})"
    )]
    InvalidBudgetConfig {
        max_total_bytes: u64,
        max_agent_bytes: u64,
    },
    #[error(
        "global runtime widget durable budget exceeded: used={used} + incoming={incoming} > max_total={max_total}"
    )]
    TotalBudgetExceeded {
        used: u64,
        incoming: u64,
        max_total: u64,
    },
    #[error(
        "per-agent runtime widget durable budget exceeded for '{agent}': used={used} + incoming={incoming} > max_agent={max_agent}"
    )]
    AgentBudgetExceeded {
        agent: String,
        used: u64,
        incoming: u64,
        max_agent: u64,
    },
    #[error("i/o error in runtime widget store: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error in runtime widget store metadata: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug)]
pub struct RuntimeWidgetStore {
    config: RuntimeWidgetStoreConfig,
    blobs_dir: PathBuf,
    meta_dir: PathBuf,
    index: HashMap<ResourceId, RuntimeWidgetAssetRecord>,
    total_bytes_used: u64,
    agent_bytes_used: HashMap<String, u64>,
}

impl RuntimeWidgetStore {
    pub fn open(config: RuntimeWidgetStoreConfig) -> Result<Self, RuntimeWidgetStoreError> {
        if config.max_total_bytes != 0
            && config.max_agent_bytes != 0
            && config.max_agent_bytes > config.max_total_bytes
        {
            return Err(RuntimeWidgetStoreError::InvalidBudgetConfig {
                max_total_bytes: config.max_total_bytes,
                max_agent_bytes: config.max_agent_bytes,
            });
        }

        fs::create_dir_all(&config.store_path)?;
        let blobs_dir = config.store_path.join("blobs");
        let meta_dir = config.store_path.join("meta");
        fs::create_dir_all(&blobs_dir)?;
        fs::create_dir_all(&meta_dir)?;

        let mut store = Self {
            config,
            blobs_dir,
            meta_dir,
            index: HashMap::new(),
            total_bytes_used: 0,
            agent_bytes_used: HashMap::new(),
        };
        store.reindex_from_disk()?;
        Ok(store)
    }

    #[cfg(test)]
    fn put_svg(
        &mut self,
        agent_namespace: &str,
        svg_bytes: &[u8],
    ) -> Result<PutOutcome, RuntimeWidgetStoreError> {
        let resource_id = ResourceId::from_content(svg_bytes);
        if self.index.contains_key(&resource_id) {
            return Ok(PutOutcome::Deduplicated { resource_id });
        }

        let incoming = svg_bytes.len() as u64;
        self.enforce_budgets(agent_namespace, incoming)?;

        let hex = resource_id.to_hex();
        let blob_path = self.blob_path_for_hex(&hex);
        let meta_path = self.meta_path_for_hex(&hex);
        let sidecar = AssetSidecar {
            resource_id_hex: hex.clone(),
            agent_namespace: agent_namespace.to_string(),
            size_bytes: incoming,
        };

        let mut wrote_blob = false;
        match fs::read(&blob_path) {
            Ok(existing_blob) => {
                if ResourceId::from_content(&existing_blob) != resource_id {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "blob path {} contains content for a different resource id",
                            blob_path.display()
                        ),
                    )
                    .into());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                write_atomic(&blob_path, svg_bytes)?;
                wrote_blob = true;
            }
            Err(err) => return Err(err.into()),
        }

        let sidecar_json = serde_json::to_vec(&sidecar)?;
        if let Err(err) = write_atomic(&meta_path, &sidecar_json) {
            if wrote_blob {
                let _ = fs::remove_file(&blob_path);
                sync_parent_dir(&blob_path);
            }
            return Err(err);
        }
        sync_parent_dir(&blob_path);
        sync_parent_dir(&meta_path);

        let record = RuntimeWidgetAssetRecord {
            size_bytes: incoming,
            agent_namespace: agent_namespace.to_string(),
        };
        self.index.insert(resource_id, record.clone());
        self.total_bytes_used = self.total_bytes_used.saturating_add(incoming);
        let agent_used = self
            .agent_bytes_used
            .entry(record.agent_namespace.clone())
            .or_insert(0);
        *agent_used = agent_used.saturating_add(incoming);

        Ok(PutOutcome::Stored { resource_id })
    }

    #[cfg(test)]
    fn contains(&self, resource_id: ResourceId) -> bool {
        self.index.contains_key(&resource_id)
    }

    #[cfg(test)]
    fn asset_count(&self) -> usize {
        self.index.len()
    }

    fn agent_bytes_used(&self, agent_namespace: &str) -> u64 {
        self.agent_bytes_used
            .get(agent_namespace)
            .copied()
            .unwrap_or(0)
    }

    fn reindex_from_disk(&mut self) -> Result<(), RuntimeWidgetStoreError> {
        let mut entries = Vec::new();
        for ent in fs::read_dir(&self.blobs_dir)? {
            let ent = ent?;
            if !ent.file_type()?.is_file() {
                continue;
            }
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".tmp-") {
                continue;
            }
            entries.push(name.to_string());
        }
        entries.sort();

        for hex in entries {
            let Some(resource_id) = parse_resource_id_hex(&hex) else {
                continue;
            };

            let blob_path = self.blob_path_for_hex(&hex);
            let blob_bytes = match fs::read(&blob_path) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if ResourceId::from_content(&blob_bytes) != resource_id {
                continue;
            }

            let meta_path = self.meta_path_for_hex(&hex);
            let sidecar: AssetSidecar = match fs::read_to_string(&meta_path)
                .ok()
                .and_then(|txt| serde_json::from_str(&txt).ok())
            {
                Some(v) => v,
                None => continue,
            };
            if sidecar.resource_id_hex != hex {
                continue;
            }
            if sidecar.size_bytes != blob_bytes.len() as u64 {
                continue;
            }

            // Re-index only if this entry still fits current configured budgets.
            if self
                .enforce_budgets(&sidecar.agent_namespace, sidecar.size_bytes)
                .is_err()
            {
                continue;
            }

            let record = RuntimeWidgetAssetRecord {
                size_bytes: sidecar.size_bytes,
                agent_namespace: sidecar.agent_namespace.clone(),
            };
            self.index.insert(resource_id, record.clone());
            self.total_bytes_used = self.total_bytes_used.saturating_add(record.size_bytes);
            *self
                .agent_bytes_used
                .entry(record.agent_namespace.clone())
                .or_insert(0) = self
                .agent_bytes_used
                .get(&record.agent_namespace)
                .copied()
                .unwrap_or(0)
                .saturating_add(record.size_bytes);
        }

        Ok(())
    }

    fn enforce_budgets(
        &self,
        agent_namespace: &str,
        incoming: u64,
    ) -> Result<(), RuntimeWidgetStoreError> {
        if self.config.max_total_bytes != 0 {
            let projected_total = self.total_bytes_used.saturating_add(incoming);
            if projected_total > self.config.max_total_bytes {
                return Err(RuntimeWidgetStoreError::TotalBudgetExceeded {
                    used: self.total_bytes_used,
                    incoming,
                    max_total: self.config.max_total_bytes,
                });
            }
        }
        if self.config.max_agent_bytes != 0 {
            let current_agent = self.agent_bytes_used(agent_namespace);
            let projected_agent = current_agent.saturating_add(incoming);
            if projected_agent > self.config.max_agent_bytes {
                return Err(RuntimeWidgetStoreError::AgentBudgetExceeded {
                    agent: agent_namespace.to_string(),
                    used: current_agent,
                    incoming,
                    max_agent: self.config.max_agent_bytes,
                });
            }
        }
        Ok(())
    }

    fn blob_path_for_hex(&self, hex: &str) -> PathBuf {
        self.blobs_dir.join(hex)
    }

    fn meta_path_for_hex(&self, hex: &str) -> PathBuf {
        self.meta_dir.join(format!("{hex}.json"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AssetSidecar {
    resource_id_hex: String,
    agent_namespace: String,
    size_bytes: u64,
}

fn parse_resource_id_hex(s: &str) -> Option<ResourceId> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(ResourceId::from_bytes(out))
}

fn hex_nibble(ch: u8) -> Option<u8> {
    match ch {
        b'0'..=b'9' => Some(ch - b'0'),
        b'a'..=b'f' => Some(ch - b'a' + 10),
        b'A'..=b'F' => Some(ch - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
fn write_atomic(final_path: &Path, bytes: &[u8]) -> Result<(), RuntimeWidgetStoreError> {
    let tmp_path = final_path.with_file_name(format!(
        ".tmp-{}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("t"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp_path, final_path)?;
    Ok(())
}

#[cfg(test)]
fn sync_parent_dir(path: &Path) {
    // Best-effort durability barrier for metadata updates. Some platforms
    // (notably Windows) may not support opening/syncing directories via
    // std::fs::File; in those cases this intentionally degrades to no-op.
    if let Some(parent) = path.parent()
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_config(path: PathBuf, max_total: u64, max_agent: u64) -> RuntimeWidgetStoreConfig {
        RuntimeWidgetStoreConfig {
            store_path: path,
            max_total_bytes: max_total,
            max_agent_bytes: max_agent,
        }
    }

    #[test]
    fn survives_restart_and_rehydrates_hash_index() {
        let temp = tempdir().unwrap();
        let cfg = make_config(temp.path().join("store"), 0, 0);
        let svg = br#"<svg width="10" height="10"></svg>"#;
        let svg_len = svg.len() as u64;

        let mut first = RuntimeWidgetStore::open(cfg.clone()).unwrap();
        let id = match first.put_svg("agent-a", svg).unwrap() {
            PutOutcome::Stored { resource_id } => resource_id,
            PutOutcome::Deduplicated { .. } => panic!("first write should not dedup"),
        };
        assert!(first.contains(id));
        assert_eq!(first.asset_count(), 1);
        drop(first);

        let mut second = RuntimeWidgetStore::open(cfg).unwrap();
        assert!(second.contains(id), "re-index must recover prior hash");
        assert_eq!(second.asset_count(), 1);
        match second.put_svg("agent-b", svg).unwrap() {
            PutOutcome::Deduplicated { resource_id } => assert_eq!(resource_id, id),
            PutOutcome::Stored { .. } => panic!("existing hash must deduplicate after restart"),
        }
        assert_eq!(second.agent_bytes_used("agent-a"), svg_len);
        assert_eq!(
            second.agent_bytes_used("agent-b"),
            0,
            "dedup hit must not consume additional durable budget"
        );
    }

    #[test]
    fn corrupt_blob_not_admitted_on_reindex() {
        let temp = tempdir().unwrap();
        let cfg = make_config(temp.path().join("store"), 0, 0);
        let svg = br#"<svg width="10" height="10"></svg>"#;
        let id = ResourceId::from_content(svg);
        let hex = id.to_hex();

        let mut first = RuntimeWidgetStore::open(cfg.clone()).unwrap();
        let _ = first.put_svg("agent-a", svg).unwrap();
        drop(first);

        let blob_path = cfg.store_path.join("blobs").join(&hex);
        fs::write(blob_path, b"corrupt").unwrap();

        let second = RuntimeWidgetStore::open(cfg).unwrap();
        assert!(
            !second.contains(id),
            "corrupted blob bytes must not be admitted into index"
        );
        assert_eq!(second.asset_count(), 0);
    }

    #[test]
    fn partial_temp_files_ignored() {
        let temp = tempdir().unwrap();
        let cfg = make_config(temp.path().join("store"), 0, 0);
        fs::create_dir_all(cfg.store_path.join("blobs")).unwrap();
        fs::create_dir_all(cfg.store_path.join("meta")).unwrap();
        fs::write(
            cfg.store_path.join("blobs").join(".tmp-incomplete"),
            b"partial",
        )
        .unwrap();

        let store = RuntimeWidgetStore::open(cfg).unwrap();
        assert_eq!(store.asset_count(), 0);
    }

    #[test]
    fn enforces_total_budget() {
        let temp = tempdir().unwrap();
        let cfg = make_config(temp.path().join("store"), 60, 0);
        let mut store = RuntimeWidgetStore::open(cfg).unwrap();

        let a = br#"<svg width="1" height="1">a</svg>"#;
        let b = br#"<svg width="1" height="1">bbbbbbbbbbbbbbbbbbbb</svg>"#;
        let _ = store.put_svg("agent-a", a).unwrap();
        let err = store.put_svg("agent-a", b).unwrap_err();
        assert!(matches!(
            err,
            RuntimeWidgetStoreError::TotalBudgetExceeded { .. }
        ));
    }

    #[test]
    fn enforces_per_agent_budget() {
        let temp = tempdir().unwrap();
        let cfg = make_config(temp.path().join("store"), 0, 80);
        let mut store = RuntimeWidgetStore::open(cfg).unwrap();

        let a = br#"<svg width="1" height="1">aaaaaaaaaaa</svg>"#;
        let b = br#"<svg width="1" height="1">bbbbbbbbbbbb</svg>"#;
        let _ = store.put_svg("agent-a", a).unwrap();
        let err = store.put_svg("agent-a", b).unwrap_err();
        assert!(matches!(
            err,
            RuntimeWidgetStoreError::AgentBudgetExceeded { .. }
        ));
    }
}
