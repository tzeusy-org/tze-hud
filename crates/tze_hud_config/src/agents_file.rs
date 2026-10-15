//! `agents.toml`: the paired-agent store.
//!
//! Each agent's table holds the SHA-256 of its PSK and its `allow` list; the
//! PSK itself is never written. The file lives next to the resolved config
//! file, or in the platform config dir when there is no config (dev).
//!
//! ```toml
//! [agents.claude]
//! psk_sha256 = "<64 lowercase hex>"
//! allow = ["*"]
//! paired_at = "2026-10-03T12:00:00Z"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tze_hud_scene::config::{AgentDirectory, ConfigError, ConfigErrorCode, PskDigest, hash_psk};

use crate::allow::{Grant, allow_to_permissions, validate_allow_entry, with_grant};
use crate::config_write::{FileMode, keep_line_endings, write_atomic};

/// File name of the agent store.
pub const AGENTS_FILE_NAME: &str = "agents.toml";

/// The parsed agent store.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsFile {
    #[serde(default)]
    pub agents: BTreeMap<String, AgentRecord>,
}

/// One paired agent.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRecord {
    /// SHA-256 of the agent's PSK, 64 lowercase hex characters.
    pub psk_sha256: String,
    /// Surfaces the agent may use: `zone:<name|*>`, `widget:<name|*>`,
    /// `portal`, `tiles`, or `*`.
    #[serde(default)]
    pub allow: Vec<String>,
    /// When the agent was paired (RFC 3339), for the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paired_at: Option<String>,
}

/// Why `agents.toml` could not be used.
#[derive(Debug)]
pub enum AgentsFileError {
    Io(std::io::Error),
    Parse(String),
    Invalid(Vec<ConfigError>),
    /// The change names an agent the file does not have.
    NoSuchAgent(String),
}

impl std::fmt::Display for AgentsFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read {AGENTS_FILE_NAME}: {e}"),
            Self::Parse(e) => write!(f, "cannot parse {AGENTS_FILE_NAME}: {e}"),
            Self::Invalid(errors) => {
                write!(f, "invalid {AGENTS_FILE_NAME}:")?;
                for e in errors {
                    write!(f, " [{}] {} (hint: {})", e.field_path, e.got, e.hint)?;
                }
                Ok(())
            }
            Self::NoSuchAgent(agent) => write!(f, "{agent} is not paired"),
        }
    }
}

impl std::error::Error for AgentsFileError {}

/// Where the agent store lives: next to `config_path`, or in the platform
/// config dir (`tze_hud/agents.toml`) with no config file.
pub fn agents_path_for(config_path: Option<&Path>) -> Option<PathBuf> {
    match config_path {
        Some(path) => Some(path.with_file_name(AGENTS_FILE_NAME)),
        None => {
            crate::resolver::xdg_config_home().map(|dir| dir.join("tze_hud").join(AGENTS_FILE_NAME))
        }
    }
}

/// Lowercase hex SHA-256 of a PSK, as stored in `psk_sha256`.
pub fn hash_psk_hex(psk: &str) -> String {
    hash_psk(psk).iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_digest(hex: &str) -> Option<PskDigest> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0u8; 32];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(digest)
}

impl AgentsFile {
    /// Parse `agents.toml` source.
    pub fn parse(src: &str) -> Result<Self, AgentsFileError> {
        toml::from_str(src).map_err(|e| AgentsFileError::Parse(e.to_string()))
    }

    /// Add or replace an agent, storing only the hash of `psk`.
    pub fn with_agent(mut self, agent_id: &str, psk: &str, allow: &[&str]) -> Self {
        self.agents.insert(
            agent_id.to_string(),
            AgentRecord {
                psk_sha256: hash_psk_hex(psk),
                allow: allow.iter().map(|a| a.to_string()).collect(),
                paired_at: None,
            },
        );
        self
    }

    /// Add every agent to `dir`, expanding `allow` lists into permissions.
    /// Returns every invalid hash and allow entry, and every hash already
    /// used by an earlier agent (in id order): one PSK must identify one agent.
    pub fn add_to(&self, dir: &mut AgentDirectory) -> Result<(), Vec<ConfigError>> {
        let mut errors = Vec::new();
        let mut owners: BTreeMap<PskDigest, &str> = BTreeMap::new();
        for (agent_id, record) in &self.agents {
            for entry in &record.allow {
                if let Err(hint) = validate_allow_entry(entry) {
                    errors.push(ConfigError {
                        code: ConfigErrorCode::UnknownAllowEntry,
                        field_path: format!("agents.{agent_id}.allow"),
                        expected: "zone:<name|*>, widget:<name|*>, portal, tiles, or *".into(),
                        got: entry.clone(),
                        hint,
                    });
                }
            }
            match parse_digest(&record.psk_sha256) {
                Some(digest) if owners.contains_key(&digest) => errors.push(ConfigError {
                    code: ConfigErrorCode::DuplicatePskHash,
                    field_path: format!("agents.{agent_id}.psk_sha256"),
                    expected: "a PSK hash no other agent uses".into(),
                    got: format!("the same hash as agents.{}", owners[&digest]),
                    hint: "remove one of the two agents, or re-pair it to get its own PSK".into(),
                }),
                Some(digest) => {
                    owners.insert(digest, agent_id);
                    dir.insert(
                        agent_id.clone(),
                        digest,
                        allow_to_permissions(&record.allow),
                    );
                }
                None => errors.push(ConfigError {
                    code: ConfigErrorCode::InvalidPskHash,
                    field_path: format!("agents.{agent_id}.psk_sha256"),
                    expected: "64 hex characters (SHA-256 of the PSK)".into(),
                    got: format!("{} characters", record.psk_sha256.len()),
                    hint: "re-pair the agent; never store the PSK itself".into(),
                }),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// The agent directory this file describes.
    pub fn directory(&self) -> Result<AgentDirectory, AgentsFileError> {
        let mut dir = AgentDirectory::default();
        self.add_to(&mut dir).map_err(AgentsFileError::Invalid)?;
        Ok(dir)
    }
}

/// Load `agents.toml`. A missing file is an empty store (nothing paired yet).
pub fn load(path: &Path) -> Result<AgentsFile, AgentsFileError> {
    match std::fs::read_to_string(path) {
        Ok(src) => AgentsFile::parse(&src),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AgentsFile::default()),
        Err(e) => Err(AgentsFileError::Io(e)),
    }
}

/// Write `agents.toml` atomically: a uniquely named temp file in the same
/// directory (removed on error), then a rename over the target. The file is
/// owner-only (0600) on Unix; Windows has no mode bits, so it takes the
/// directory's ACL.
pub fn save_atomic(path: &Path, file: &AgentsFile) -> std::io::Result<()> {
    let src = toml::to_string(file).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(path, src.as_bytes(), FileMode::Private, |from, to| {
        std::fs::rename(from, to)
    })
}

/// One change to `agents.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentChange {
    /// Pair an agent, or re-pair one (a new hash replaces the old key).
    Pair { agent: String, record: AgentRecord },
    /// Forget an agent: its PSK stops authenticating. Forgetting an agent the
    /// file does not have changes nothing.
    Unpair(String),
    /// Grant or withdraw one surface class in an agent's allow list, as it
    /// stands in the file (see [`with_grant`]).
    Grant {
        agent: String,
        grant: Grant,
        on: bool,
    },
}

/// The store either side of one [`edit`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edited {
    pub before: AgentsFile,
    pub after: AgentsFile,
}

/// Apply `change` to the `agents.toml` at `path` (a missing file is an empty
/// store) and return the store before and as written.
///
/// The file is re-read first, so hand edits made since startup are kept, and
/// it is edited in place with `toml_edit`: comments, key order and other
/// agents' tables survive. The result is validated before anything is
/// written, then replaced atomically (owner-only, like [`save_atomic`]).
/// Callers that write concurrently must serialize their calls; this function
/// holds no lock of its own.
pub fn edit(path: &Path, change: &AgentChange) -> Result<Edited, AgentsFileError> {
    let src = match std::fs::read_to_string(path) {
        Ok(src) => src,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(AgentsFileError::Io(e)),
    };
    let before = AgentsFile::parse(&src)?;
    let out = apply_change(&src, change)?;
    let after = AgentsFile::parse(&out)?;
    after.directory()?;
    if out != src {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(AgentsFileError::Io)?;
        }
        write_atomic(path, out.as_bytes(), FileMode::Private, |from, to| {
            std::fs::rename(from, to)
        })
        .map_err(AgentsFileError::Io)?;
    }
    Ok(Edited { before, after })
}

fn string_array(items: &[String]) -> toml_edit::Value {
    toml_edit::Value::Array(items.iter().map(String::as_str).collect())
}

/// Set `key` in `table`, keeping the decoration (comments, spacing) of an
/// existing value.
fn set_key(table: &mut toml_edit::Table, key: &str, value: toml_edit::Value) {
    match table.get_mut(key).and_then(toml_edit::Item::as_value_mut) {
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = value;
            *existing.decor_mut() = decor;
        }
        None => {
            table.insert(key, toml_edit::Item::Value(value));
        }
    }
}

/// Apply `change` to the TOML text `src`.
fn apply_change(src: &str, change: &AgentChange) -> Result<String, AgentsFileError> {
    use toml_edit::{DocumentMut, Item, Table};
    let mut doc: DocumentMut = src
        .parse()
        .map_err(|e: toml_edit::TomlError| AgentsFileError::Parse(e.to_string()))?;
    let not_a_table = || AgentsFileError::Parse("[agents] is not a table".to_owned());
    let agents = doc
        .entry("agents")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_mut()
        .ok_or_else(not_a_table)?;
    let missing = |agent: &str| AgentsFileError::NoSuchAgent(agent.to_owned());
    match change {
        AgentChange::Pair { agent, record } => {
            let table = agents
                .entry(agent)
                .or_insert_with(|| Item::Table(Table::new()))
                .as_table_mut()
                .ok_or_else(not_a_table)?;
            set_key(table, "psk_sha256", record.psk_sha256.as_str().into());
            set_key(table, "allow", string_array(&record.allow));
            match &record.paired_at {
                Some(at) => set_key(table, "paired_at", at.as_str().into()),
                None => {
                    table.remove("paired_at");
                }
            }
        }
        AgentChange::Unpair(agent) => {
            agents.remove(agent);
        }
        AgentChange::Grant { agent, grant, on } => {
            let table = agents
                .get_mut(agent)
                .ok_or_else(|| missing(agent))?
                .as_table_mut()
                .ok_or_else(not_a_table)?;
            let allow: Vec<String> = table
                .get("allow")
                .and_then(toml_edit::Item::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let changed = with_grant(&allow, *grant, *on);
            if changed != allow {
                set_key(table, "allow", string_array(&changed));
            }
        }
    }
    Ok(keep_line_endings(src, doc.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PSK: &str = "3f1c0a9e4b7d2c6f8a0e1b3d5f7092a4c6e8f0b2d4a6c8e0f2a4b6c8d0e2f4a6";

    #[test]
    fn agents_file_round_trip_stores_only_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("tze_hud.toml");
        let path = agents_path_for(Some(&config)).unwrap();
        assert_eq!(path, dir.path().join(AGENTS_FILE_NAME));

        let file = AgentsFile::default().with_agent("claude", PSK, &["zone:*", "portal"]);
        save_atomic(&path, &file).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains(PSK), "agents.toml must not contain the PSK");
        assert!(saved.contains(&hash_psk_hex(PSK)));
        assert_eq!(load(&path).unwrap(), file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "agents.toml must be owner-only");
        }
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, [AGENTS_FILE_NAME], "no temp file left behind");

        let id = file.directory().unwrap().resolve(PSK, "").unwrap();
        assert_eq!(id.agent_id, "claude");
        assert!(id.allows("publish_zone:subtitle"));
        assert!(!id.allows("create_tiles"));
    }

    #[test]
    fn agents_file_missing_is_empty_and_bad_entries_are_config_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load(&dir.path().join(AGENTS_FILE_NAME))
                .unwrap()
                .agents
                .is_empty()
        );

        let file =
            AgentsFile::parse("[agents.a]\npsk_sha256 = \"abc\"\nallow = [\"create_tiles\"]\n")
                .unwrap();
        let Err(AgentsFileError::Invalid(errors)) = file.directory() else {
            panic!("expected validation errors");
        };
        let codes: Vec<_> = errors.iter().map(|e| e.code.clone()).collect();
        assert_eq!(
            codes,
            [
                ConfigErrorCode::UnknownAllowEntry,
                ConfigErrorCode::InvalidPskHash
            ]
        );
        assert!(AgentsFile::parse("[agents.a]\npsk = \"plaintext\"\n").is_err());
    }

    #[test]
    fn a_psk_hash_shared_by_two_agents_is_rejected() {
        let shared = AgentsFile::default()
            .with_agent("a", PSK, &["*"])
            .with_agent("b", PSK, &["*"]);
        let errors = shared
            .add_to(&mut AgentDirectory::default())
            .expect_err("duplicate hash on add");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, ConfigErrorCode::DuplicatePskHash);
        assert_eq!(errors[0].field_path, "agents.b.psk_sha256");

        // The same file from disk fails strict validation, naming both agents.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AGENTS_FILE_NAME);
        save_atomic(&path, &shared).unwrap();
        let err = load(&path).unwrap().directory().unwrap_err().to_string();
        assert!(
            err.contains("agents.b.psk_sha256") && err.contains("agents.a"),
            "{err}"
        );
    }

    /// Panel and pairing edits keep the owner's comments and other agents'
    /// tables, re-read the file each time, and validate before writing
    /// (hud-vniri.6).
    #[test]
    fn edits_keep_comments_and_other_agents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AGENTS_FILE_NAME);
        let hash = hash_psk_hex(PSK);
        let other = hash_psk_hex("another-psk");
        std::fs::write(
            &path,
            format!(
                "# my agents\n[agents.a] # laptop\npsk_sha256 = \"{hash}\"\nallow = [\"*\"] # all\n\n\
                 [agents.b]\npsk_sha256 = \"{other}\"\nallow = [\"tiles\"]\n"
            ),
        )
        .unwrap();

        // A grant edits the list as it stands in the file, so the hand-made
        // single-zone entry survives withdrawing `*`'s zones.
        std::fs::write(
            &path,
            std::fs::read_to_string(&path).unwrap().replace(
                "allow = [\"*\"] # all",
                "allow = [\"*\", \"zone:subtitle\"] # all",
            ),
        )
        .unwrap();
        let edited = edit(
            &path,
            &AgentChange::Grant {
                agent: "a".into(),
                grant: Grant::Zones,
                on: false,
            },
        )
        .unwrap();
        assert_eq!(edited.before.agents["a"].allow, ["*", "zone:subtitle"]);
        assert_eq!(
            edited.after.agents["a"].allow,
            ["widget:*", "portal", "tiles", "zone:subtitle"]
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.starts_with("# my agents\n[agents.a] # laptop\n"),
            "{saved}"
        );
        assert!(saved.contains("\"zone:subtitle\"] # all"), "{saved}");

        let file = edit(&path, &AgentChange::Unpair("b".into())).unwrap().after;
        assert_eq!(file.agents.keys().collect::<Vec<_>>(), ["a"]);
        let file = edit(
            &path,
            &AgentChange::Pair {
                agent: "c".into(),
                record: AgentRecord {
                    psk_sha256: other.clone(),
                    allow: vec!["*".into()],
                    paired_at: Some("2026-10-15T00:00:00Z".into()),
                },
            },
        )
        .unwrap()
        .after;
        assert_eq!(file.agents.len(), 2);
        assert_eq!(load(&path).unwrap(), file);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# my agents")
        );

        // Nothing is written when the result would not load, or names an
        // agent the file lacks; unpairing a missing agent changes nothing.
        let before = std::fs::read_to_string(&path).unwrap();
        let dup = AgentChange::Pair {
            agent: "d".into(),
            record: AgentRecord {
                psk_sha256: other,
                allow: vec![],
                paired_at: None,
            },
        };
        assert!(matches!(
            edit(&path, &dup),
            Err(AgentsFileError::Invalid(_))
        ));
        let grant_missing = AgentChange::Grant {
            agent: "zz".into(),
            grant: Grant::Tiles,
            on: true,
        };
        assert!(matches!(
            edit(&path, &grant_missing),
            Err(AgentsFileError::NoSuchAgent(_))
        ));
        let edited = edit(&path, &AgentChange::Unpair("zz".into())).unwrap();
        assert_eq!(edited.before, edited.after);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn edit_creates_a_missing_store_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(AGENTS_FILE_NAME);
        let record = AgentRecord {
            psk_sha256: hash_psk_hex(PSK),
            allow: vec!["*".into()],
            paired_at: None,
        };
        let file = edit(
            &path,
            &AgentChange::Pair {
                agent: "a".into(),
                record,
            },
        )
        .unwrap()
        .after;
        assert_eq!(load(&path).unwrap(), file);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.starts_with("[agents.a]"),
            "no empty [agents] header: {saved}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
