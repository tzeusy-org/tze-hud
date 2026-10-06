//! Persist `[design_tokens]` choices (theme, fonts) to `tze_hud.toml`.
//!
//! The only writer of `tze_hud.toml`. It edits just the named keys with
//! `toml_edit`, so the owner's comments, key order, quoting and every other
//! section survive byte-for-byte, and replaces the file atomically. Everything
//! is validated before anything is written.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::raw::{RawConfig, RawDesignTokens};
use crate::themes::{THEME_KEY, validate_theme};

/// Keys the settings card may write.
pub const WRITABLE_KEYS: [&str; 4] = [THEME_KEY, "font.sans", "font.mono", "font.serif"];

/// What to do with one `[design_tokens]` key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenUpdate {
    /// Set the key to this string.
    Set(String),
    /// Delete the key (e.g. the "Default" font row removes `font.sans`).
    Remove,
}

#[derive(Debug)]
pub enum ConfigWriteError {
    /// The key is not one the settings card writes.
    UnknownKey(String),
    /// Unknown theme or invalid font value; nothing was written.
    InvalidValue {
        key: String,
        expected: String,
        got: String,
    },
    /// The config file does not exist (it is never created here).
    NotFound(PathBuf),
    /// The existing file is not valid TOML.
    Parse(String),
    /// The file is read-only or locked by another process.
    ReadOnlyOrLocked(PathBuf),
    Io(std::io::Error),
}

impl std::fmt::Display for ConfigWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownKey(k) => write!(f, "{k:?} is not a settable design token"),
            Self::InvalidValue { key, expected, got } => {
                write!(f, "invalid {key}: expected {expected}, got {got}")
            }
            Self::NotFound(p) => write!(f, "config file {} does not exist", p.display()),
            Self::Parse(e) => write!(f, "config file is not valid TOML: {e}"),
            Self::ReadOnlyOrLocked(p) => {
                write!(f, "config file {} is read-only or locked", p.display())
            }
            Self::Io(e) => write!(f, "config write failed: {e}"),
        }
    }
}

impl std::error::Error for ConfigWriteError {}

impl From<std::io::Error> for ConfigWriteError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            // The path is filled in by the caller that knows it.
            Self::ReadOnlyOrLocked(PathBuf::new())
        } else {
            Self::Io(e)
        }
    }
}

/// Check every update; no I/O.
fn validate(updates: &[(&str, TokenUpdate)]) -> Result<(), ConfigWriteError> {
    for (key, update) in updates {
        if !WRITABLE_KEYS.contains(key) {
            return Err(ConfigWriteError::UnknownKey((*key).to_owned()));
        }
        let TokenUpdate::Set(value) = update else {
            continue;
        };
        if *key == THEME_KEY {
            let raw = RawConfig {
                design_tokens: Some(RawDesignTokens(
                    [(THEME_KEY.to_owned(), value.clone())].into(),
                )),
                ..RawConfig::default()
            };
            let mut errors = Vec::new();
            validate_theme(&raw, &mut errors);
            if let Some(e) = errors.into_iter().next() {
                return Err(ConfigWriteError::InvalidValue {
                    key: (*key).to_owned(),
                    expected: e.expected,
                    got: e.got,
                });
            }
        } else if value.trim().is_empty() || value.chars().any(char::is_control) {
            return Err(ConfigWriteError::InvalidValue {
                key: (*key).to_owned(),
                expected: "a non-empty font family name".to_owned(),
                got: format!("{value:?}"),
            });
        }
    }
    Ok(())
}

/// Apply `updates` to `[design_tokens]` of the TOML text `src`.
fn apply(src: &str, updates: &[(&str, TokenUpdate)]) -> Result<String, ConfigWriteError> {
    let mut doc: DocumentMut = src
        .parse()
        .map_err(|e: toml_edit::TomlError| ConfigWriteError::Parse(e.to_string()))?;
    for (key, update) in updates {
        match update {
            TokenUpdate::Remove => {
                if let Some(t) = doc.get_mut("design_tokens").and_then(Item::as_table_mut) {
                    t.remove(key);
                }
            }
            TokenUpdate::Set(value) => {
                let item = doc
                    .entry("design_tokens")
                    .or_insert_with(|| Item::Table(Table::new()));
                let table = item.as_table_mut().ok_or_else(|| {
                    ConfigWriteError::Parse("[design_tokens] is not a table".to_owned())
                })?;
                match table.get_mut(key).and_then(Item::as_value_mut) {
                    // Keep the trailing comment / spacing of an existing value.
                    Some(existing) => {
                        let decor = existing.decor().clone();
                        *existing = Value::from(value.as_str());
                        *existing.decor_mut() = decor;
                    }
                    None => {
                        table.insert(key, toml_edit::value(value.as_str()));
                    }
                }
            }
        }
    }
    let mut out = doc.to_string();
    // New lines toml_edit adds are LF; keep a CRLF file CRLF. A file with mixed
    // endings is left as edited.
    if src.contains("\r\n") && !src.replace("\r\n", "").contains('\n') {
        out = out.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    Ok(out)
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// How [`write_atomic`] sets the replacement file's permissions.
#[derive(Clone, Copy)]
pub(crate) enum FileMode {
    /// Keep the permissions of the file being replaced.
    Inherit,
    /// Owner read/write only (0600) on Unix. Windows has no mode bits; the
    /// file takes the directory's ACL.
    Private,
}

/// Write `bytes` to a unique temp file beside `path`, fsync it, then `rename`
/// it over `path`. The temp file is removed if any step fails.
pub(crate) fn write_atomic(
    path: &Path,
    bytes: &[u8],
    mode: FileMode,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp_name = format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = dir.map_or_else(|| PathBuf::from(&tmp_name), |d| d.join(&tmp_name));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if matches!(mode, FileMode::Private) {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if matches!(mode, FileMode::Inherit)
            && let Ok(meta) = std::fs::metadata(path)
        {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        drop(f);
        rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn set_design_tokens_with(
    config_path: &Path,
    updates: &[(&str, TokenUpdate)],
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), ConfigWriteError> {
    validate(updates)?;
    let locked = |e: ConfigWriteError| match e {
        ConfigWriteError::ReadOnlyOrLocked(_) => {
            ConfigWriteError::ReadOnlyOrLocked(config_path.to_owned())
        }
        other => other,
    };
    let src = match std::fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConfigWriteError::NotFound(config_path.to_owned()));
        }
        Err(e) => return Err(locked(e.into())),
    };
    let out = apply(&src, updates)?;
    if out == src {
        return Ok(());
    }
    if std::fs::metadata(config_path).is_ok_and(|m| m.permissions().readonly()) {
        return Err(ConfigWriteError::ReadOnlyOrLocked(config_path.to_owned()));
    }
    write_atomic(config_path, out.as_bytes(), FileMode::Inherit, rename)
        .map_err(|e| locked(e.into()))
}

/// Set or remove `[design_tokens]` keys (`theme`, `font.sans`, `font.mono`,
/// `font.serif`) in the config file at `config_path`.
///
/// Validates every update first and writes nothing on error. Only the named
/// keys change; the table is created at the end of the file if missing. The
/// replacement is atomic (temp file in the same directory, fsync, rename).
/// A missing file is [`ConfigWriteError::NotFound`]; a read-only or locked
/// one is [`ConfigWriteError::ReadOnlyOrLocked`].
pub fn set_design_tokens(
    config_path: &Path,
    updates: &[(&str, TokenUpdate)],
) -> Result<(), ConfigWriteError> {
    set_design_tokens_with(config_path, updates, |from, to| std::fs::rename(from, to))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(k: &str, v: &str) -> (String, TokenUpdate) {
        (k.to_owned(), TokenUpdate::Set(v.to_owned()))
    }

    fn run(src: &str, updates: &[(String, TokenUpdate)]) -> std::io::Result<String> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tze_hud.toml");
        std::fs::write(&path, src)?;
        let refs: Vec<(&str, TokenUpdate)> = updates
            .iter()
            .map(|(k, u)| (k.as_str(), u.clone()))
            .collect();
        set_design_tokens(&path, &refs).map_err(std::io::Error::other)?;
        std::fs::read_to_string(&path)
    }

    const DOC: &str = "# my HUD\n[runtime]\nprofile = \"full-display\"  # keep\n\n[design_tokens]\n# pick one\ntheme = \"classic\" # trailing\n\"font.sans\" = \"Inter\"\n\"space.md\" = \"12\"\n\n[[tabs]]\nname = \"Main\"\n";

    #[test]
    fn edits_only_named_keys_and_keeps_everything_else_byte_for_byte() {
        let got = run(DOC, &[set("theme", "blueprint")]).unwrap();
        assert_eq!(got, DOC.replace("\"classic\"", "\"blueprint\""));
        let got = run(DOC, &[set("font.mono", "Consolas")]).unwrap();
        assert_eq!(
            got,
            DOC.replace(
                "\"space.md\" = \"12\"\n",
                "\"space.md\" = \"12\"\n\"font.mono\" = \"Consolas\"\n"
            )
        );
    }

    #[test]
    fn creates_a_missing_design_tokens_table_at_the_end() {
        let src = "[runtime]\nprofile = \"full-display\"\n";
        let got = run(src, &[set("theme", "classic")]).unwrap();
        assert!(got.starts_with(src), "{got}");
        let parsed: toml::Table = toml::from_str(&got).unwrap();
        assert_eq!(parsed["design_tokens"]["theme"].as_str(), Some("classic"));
        // Also works when the last line has no newline.
        let got = run("[runtime]\nprofile = \"x\"", &[set("theme", "classic")]).unwrap();
        assert!(toml::from_str::<toml::Table>(&got).is_ok(), "{got}");
    }

    #[test]
    fn invalid_theme_or_font_is_rejected_and_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tze_hud.toml");
        std::fs::write(&path, DOC).unwrap();
        for bad in [
            vec![("theme", TokenUpdate::Set("nope".into()))],
            vec![("font.sans", TokenUpdate::Set("  ".into()))],
            vec![("space.md", TokenUpdate::Set("1".into()))],
            // One bad update poisons the whole batch.
            vec![
                ("font.sans", TokenUpdate::Set("Fine".into())),
                ("theme", TokenUpdate::Set("nope".into())),
            ],
        ] {
            assert!(set_design_tokens(&path, &bad).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), DOC);
        }
        let entries = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(entries, 1, "no temp file left behind");
    }

    #[test]
    fn crlf_files_stay_crlf() {
        let src = DOC.replace('\n', "\r\n");
        let got = run(&src, &[set("font.mono", "Consolas")]).unwrap();
        assert!(
            !got.replace("\r\n", "").contains('\n'),
            "lone LF in {got:?}"
        );
        assert!(got.contains("\"font.mono\" = \"Consolas\"\r\n"));
        assert_eq!(got.replace("\"font.mono\" = \"Consolas\"\r\n", ""), src);
    }

    #[test]
    fn temp_file_is_removed_when_the_rename_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tze_hud.toml");
        std::fs::write(&path, DOC).unwrap();
        let err = set_design_tokens_with(
            &path,
            &[("theme", TokenUpdate::Set("blueprint".into()))],
            |_, _| Err(std::io::Error::other("boom")),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigWriteError::Io(_)), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DOC);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn remove_deletes_the_key_and_is_a_no_op_when_absent() {
        let got = run(DOC, &[("font.sans".to_owned(), TokenUpdate::Remove)]).unwrap();
        assert_eq!(got, DOC.replace("\"font.sans\" = \"Inter\"\n", ""));
        let src = "[runtime]\nprofile = \"x\"\n";
        let got = run(src, &[("font.sans".to_owned(), TokenUpdate::Remove)]).unwrap();
        assert_eq!(got, src);
    }

    #[test]
    fn missing_file_is_a_typed_error_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tze_hud.toml");
        let err =
            set_design_tokens(&path, &[("theme", TokenUpdate::Set("classic".into()))]).unwrap_err();
        assert!(matches!(err, ConfigWriteError::NotFound(_)));
        assert!(!path.exists());
    }
}
