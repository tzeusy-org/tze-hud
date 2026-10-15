//! The paired-agent store: `agents.toml` on disk and the live directory MCP
//! and gRPC authenticate against, changed together.
//!
//! Pairing (`POST /pair`) and the control panel (unpair, allow lists) are the
//! writers. Every change takes one lock, re-reads the file (so a hand edit
//! made while the HUD runs is kept), edits it in place keeping comments,
//! writes it atomically and only then swaps the live directory. A change that
//! fails leaves both untouched. Reading the store notices a hand edit by the
//! file's modification time and loads it into the live directory too, so a
//! hand edit applies from the next change or the next read (the panel reads
//! once a second while open), whichever comes first.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tze_hud_config::agents_file::{self, AgentChange, AgentsFile, Edited};
use tze_hud_scene::config::SharedAgents;

pub(crate) struct AgentStore {
    path: Option<PathBuf>,
    live: SharedAgents,
    /// The file as last read or written. The mutex also serializes writers.
    cached: Mutex<Cached>,
}

struct Cached {
    file: AgentsFile,
    /// The file's modification time and length when `file` was read
    /// (`None`: missing).
    stamp: Option<(SystemTime, u64)>,
}

impl std::fmt::Debug for AgentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl AgentStore {
    /// The store at `path` (`None`: nowhere to write, so changes fail) over
    /// the live directory the runtime already loaded from it.
    pub(crate) fn open(path: Option<PathBuf>, live: SharedAgents) -> Self {
        let stamp = path.as_deref().and_then(modified);
        let file = path
            .as_deref()
            .map(agents_file::load)
            .transpose()
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "agents.toml unreadable; the panel lists no paired agents");
                None
            })
            .unwrap_or_default();
        Self {
            path,
            live,
            cached: Mutex::new(Cached { file, stamp }),
        }
    }

    /// The paired agents, reloaded (into the live directory too) when the
    /// file changed on disk since it was last read or written. Blocking file
    /// I/O: call it off async workers. A file that no
    /// longer loads is reported once and the last good store kept.
    pub(crate) fn file(&self) -> AgentsFile {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(path) = self.path.as_deref() {
            let stamp = modified(path);
            if stamp != cached.stamp {
                cached.stamp = stamp;
                match agents_file::load(path).and_then(|file| Ok((file.directory()?, file))) {
                    Ok((paired, file)) => {
                        tracing::info!(
                            agents = file.agents.len(),
                            "agents.toml changed on disk; reloaded"
                        );
                        self.live
                            .store(Arc::new(self.live.load().with_agents_of(paired)));
                        cached.file = file;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "agents.toml changed on disk but does not load; keeping the last good store");
                    }
                }
            }
        }
        cached.file.clone()
    }

    /// Apply `change` to `agents.toml`, then to the live directory. Returns
    /// the store before and as written. Blocking file I/O: call it off async
    /// workers.
    pub(crate) fn apply(&self, change: &AgentChange) -> Result<Edited, String> {
        let path = self
            .path
            .as_deref()
            .ok_or("there is no agents.toml location")?;
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        let edited = agents_file::edit(path, change).map_err(|e| e.to_string())?;
        let paired = edited.after.directory().map_err(|e| e.to_string())?;
        self.live
            .store(Arc::new(self.live.load().with_agents_of(paired)));
        *cached = Cached {
            file: edited.after.clone(),
            stamp: modified(path),
        };
        Ok(edited)
    }
}

/// What tells a hand edit apart: modification time and length.
fn modified(path: &std::path::Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_config::Grant;
    use tze_hud_config::agents_file::{AgentRecord, hash_psk_hex};
    use tze_hud_scene::config::AgentDirectory;

    fn pair(agent: &str, psk: &str) -> AgentChange {
        AgentChange::Pair {
            agent: agent.into(),
            record: AgentRecord {
                psk_sha256: hash_psk_hex(psk),
                allow: vec!["*".into()],
                paired_at: None,
            },
        }
    }

    /// Pairing redeems and panel edits racing on one store lose nothing:
    /// every pair lands, the last allow list wins, the owner's comment
    /// survives, and the live directory matches the file (hud-vniri.6).
    #[test]
    fn racing_writers_lose_nothing_and_keep_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.toml");
        std::fs::write(
            &path,
            format!(
                "# owner notes\n[agents.base]\npsk_sha256 = \"{}\"\nallow = [\"*\"]\n",
                hash_psk_hex("base-psk")
            ),
        )
        .unwrap();
        let live = AgentDirectory::unrestricted("dev-psk").shared();
        let store = Arc::new(AgentStore::open(Some(path.clone()), Arc::clone(&live)));

        std::thread::scope(|scope| {
            for i in 0..8 {
                let (pairing, panel) = (Arc::clone(&store), Arc::clone(&store));
                scope.spawn(move || {
                    pairing
                        .apply(&pair(&format!("agent-{i}"), &format!("psk-{i}")))
                        .unwrap();
                });
                scope.spawn(move || {
                    let grant = if i % 2 == 0 {
                        Grant::Admin
                    } else {
                        Grant::Portal
                    };
                    panel
                        .apply(&AgentChange::Grant {
                            agent: "base".into(),
                            grant,
                            on: true,
                        })
                        .unwrap();
                });
            }
        });

        let file = agents_file::load(&path).unwrap();
        assert_eq!(file.agents.len(), 9, "every pairing landed");
        assert_eq!(
            file.agents["base"].allow,
            ["*", "admin"],
            "grants applied once"
        );
        assert_eq!(store.file(), file, "the cache is the file");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("# owner notes\n")
        );
        let directory = live.load();
        for i in 0..8 {
            let id = directory.resolve(&format!("psk-{i}"), "").unwrap();
            assert_eq!(id.agent_id, format!("agent-{i}"));
        }
        assert!(
            directory.resolve("dev-psk", "anyone").is_ok(),
            "the dev PSK survives"
        );

        store.apply(&AgentChange::Unpair("agent-3".into())).unwrap();
        assert!(
            live.load().resolve("psk-3", "").is_err(),
            "an unpaired key stops working"
        );
        let again = store.apply(&AgentChange::Unpair("agent-3".into())).unwrap();
        assert_eq!(again.before, again.after, "unpairing twice is harmless");
    }

    /// A hand edit made while the HUD runs is neither lost by the next panel
    /// change nor invisible until one: reading the store picks it up and the
    /// live directory follows (hud-vniri.6).
    #[test]
    fn hand_edits_survive_panel_changes_and_reach_the_live_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.toml");
        let entry = |agent: &str, psk: &str| {
            format!(
                "[agents.{agent}]\npsk_sha256 = \"{}\"\nallow = [\"*\"]\n",
                hash_psk_hex(psk)
            )
        };
        std::fs::write(&path, entry("a", "psk-a")).unwrap();
        let live = AgentDirectory::default().shared();
        let store = AgentStore::open(Some(path.clone()), Arc::clone(&live));
        store.file();

        // Hand-add `b`, then toggle a grant on `a` without reading first.
        let hand = format!("{}\n{}", entry("a", "psk-a"), entry("b", "psk-b"));
        std::fs::write(&path, &hand).unwrap();
        let edited = store
            .apply(&AgentChange::Grant {
                agent: "a".into(),
                grant: Grant::Tiles,
                on: false,
            })
            .unwrap();
        assert!(
            edited.before.agents.contains_key("b"),
            "edits start from disk"
        );
        assert!(
            edited.after.agents.contains_key("b"),
            "the hand entry survives"
        );
        assert!(live.load().resolve("psk-b", "").is_ok());

        // A hand removal reaches the live directory on the next read.
        std::fs::write(&path, entry("b", "psk-b")).unwrap();
        let bumped = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(bumped)
            .unwrap();
        assert_eq!(store.file().agents.keys().collect::<Vec<_>>(), ["b"]);
        assert!(
            live.load().resolve("psk-a", "").is_err(),
            "a's key stops working"
        );

        // A broken hand edit keeps the last good store.
        std::fs::write(&path, "not toml [").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(bumped + std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(store.file().agents.keys().collect::<Vec<_>>(), ["b"]);
        assert!(live.load().resolve("psk-b", "").is_ok());
    }

    #[test]
    fn a_store_without_a_path_refuses_changes() {
        let store = AgentStore::open(None, SharedAgents::default());
        assert!(store.apply(&pair("a", "p")).is_err());
        assert!(store.file().agents.is_empty());
    }
}
