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

use crate::allow::{allow_to_permissions, validate_allow_entry};

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
    /// Returns every invalid hash and allow entry.
    pub fn add_to(&self, dir: &mut AgentDirectory) -> Result<(), Vec<ConfigError>> {
        let mut errors = Vec::new();
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
                Some(digest) => {
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

/// Write `agents.toml` atomically: a temp file in the same directory, then a
/// rename over the target.
pub fn save_atomic(path: &Path, file: &AgentsFile) -> std::io::Result<()> {
    let src = toml::to_string(file).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, src)?;
    std::fs::rename(&tmp, path)
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
}
