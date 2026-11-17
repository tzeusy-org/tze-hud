//! Runtime widget SVG asset store configuration helpers.
//!
//! Resolves the configured widget asset root and validates persisted SVG assets.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use tze_hud_scene::config::{ConfigError, ConfigErrorCode};

use crate::raw::RawConfig;

/// Default global durable footprint ceiling (256 MiB).
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// Default per-agent durable footprint ceiling (64 MiB).
pub const DEFAULT_MAX_AGENT_BYTES: u64 = 64 * 1024 * 1024;
/// Default runtime widget asset store directory (relative to cache root).
pub const DEFAULT_STORE_DIRNAME: &str = "runtime_widget_assets";

/// Resolved runtime widget asset store settings used by runtime startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeWidgetAssetStoreConfig {
    pub store_path: PathBuf,
    pub max_total_bytes: u64,
    pub max_agent_bytes: u64,
}

/// Validate runtime widget asset budget relationship in raw config.
pub fn validate_runtime_widget_asset_budgets(raw: &RawConfig, errors: &mut Vec<ConfigError>) {
    if let Some(section) = raw.widget_runtime_assets.as_ref() {
        let max_total = section.max_total_bytes.unwrap_or(DEFAULT_MAX_TOTAL_BYTES);
        let max_agent = section.max_agent_bytes.unwrap_or(DEFAULT_MAX_AGENT_BYTES);
        if max_total != 0 && max_agent != 0 && max_agent > max_total {
            errors.push(ConfigError {
                code: ConfigErrorCode::Other("CONFIG_WIDGET_ASSET_BUDGET_INVALID".into()),
                field_path: "widget_runtime_assets".into(),
                expected: "max_agent_bytes <= max_total_bytes (or 0 for unbounded)".into(),
                got: format!("max_agent_bytes={max_agent}, max_total_bytes={max_total}"),
                hint: "set max_agent_bytes <= max_total_bytes, or set one to 0 for unbounded"
                    .into(),
            });
        }
    }
}

/// Resolve and validate runtime widget asset store settings.
///
/// Path rules:
/// - Explicit `store_path` uses config-parent-relative resolution for relative paths.
/// - Absent `store_path` falls back to platform default cache location.
///
/// Validation rules:
/// - `max_agent_bytes <= max_total_bytes` unless either side is `0` (unbounded).
/// - `store_path` must be creatable/writable.
pub fn resolve_runtime_widget_asset_store(
    raw: &RawConfig,
    config_parent: Option<&Path>,
) -> Result<RuntimeWidgetAssetStoreConfig, ConfigError> {
    let section = raw.widget_runtime_assets.as_ref();
    let max_total = section
        .and_then(|v| v.max_total_bytes)
        .unwrap_or(DEFAULT_MAX_TOTAL_BYTES);
    let max_agent = section
        .and_then(|v| v.max_agent_bytes)
        .unwrap_or(DEFAULT_MAX_AGENT_BYTES);

    if max_total != 0 && max_agent != 0 && max_agent > max_total {
        return Err(ConfigError {
            code: ConfigErrorCode::Other("CONFIG_WIDGET_ASSET_BUDGET_INVALID".into()),
            field_path: "widget_runtime_assets".into(),
            expected: "max_agent_bytes <= max_total_bytes (or 0 for unbounded)".into(),
            got: format!("max_agent_bytes={max_agent}, max_total_bytes={max_total}"),
            hint: "set max_agent_bytes <= max_total_bytes, or set one to 0 for unbounded".into(),
        });
    }

    let store_path = resolve_store_path(raw, config_parent);
    ensure_writable_store_path(&store_path)?;

    Ok(RuntimeWidgetAssetStoreConfig {
        store_path,
        max_total_bytes: max_total,
        max_agent_bytes: max_agent,
    })
}

/// Resolve store path from raw config + optional config parent.
pub fn resolve_store_path(raw: &RawConfig, config_parent: Option<&Path>) -> PathBuf {
    if let Some(path_str) = raw
        .widget_runtime_assets
        .as_ref()
        .and_then(|v| v.store_path.as_deref())
    {
        let path = Path::new(path_str);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            config_parent
                .unwrap_or_else(|| Path::new("."))
                .join(path)
                .to_path_buf()
        }
    } else {
        platform_default_store_path()
    }
}

fn ensure_writable_store_path(store_path: &Path) -> Result<(), ConfigError> {
    fs::create_dir_all(store_path).map_err(|err| ConfigError {
        code: ConfigErrorCode::Other("CONFIG_WIDGET_ASSET_STORE_UNWRITABLE".into()),
        field_path: "widget_runtime_assets.store_path".into(),
        expected: "writable directory".into(),
        got: format!("{} ({err})", store_path.display()),
        hint: "ensure the store directory exists and is writable".into(),
    })?;

    let probe = store_path.join(format!(
        ".tze_hud_runtime_widget_assets_write_probe_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|err| ConfigError {
            code: ConfigErrorCode::Other("CONFIG_WIDGET_ASSET_STORE_UNWRITABLE".into()),
            field_path: "widget_runtime_assets.store_path".into(),
            expected: "writable directory".into(),
            got: format!("{} ({err})", store_path.display()),
            hint: "ensure the store directory exists and is writable".into(),
        })?;
    if let Err(err) = file.write_all(b"probe") {
        drop(file);
        let _ = fs::remove_file(&probe);
        return Err(ConfigError {
            code: ConfigErrorCode::Other("CONFIG_WIDGET_ASSET_STORE_UNWRITABLE".into()),
            field_path: "widget_runtime_assets.store_path".into(),
            expected: "writable directory".into(),
            got: format!("{} ({err})", store_path.display()),
            hint: "ensure the store directory exists and is writable".into(),
        });
    }
    drop(file);
    let _ = fs::remove_file(&probe);
    Ok(())
}

fn platform_default_store_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(local_app_data)
                .join("tze_hud")
                .join("resources")
                .join(DEFAULT_STORE_DIRNAME);
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Caches")
                .join("tze_hud")
                .join("resources")
                .join(DEFAULT_STORE_DIRNAME);
        }
    }

    let cache_root = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|_| PathBuf::from("/tmp"));

    cache_root
        .join("tze_hud")
        .join("resources")
        .join(DEFAULT_STORE_DIRNAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::{RawConfig, RawWidgetRuntimeAssets};
    use tempfile::tempdir;

    #[test]
    fn budget_relationship_rejected_when_agent_exceeds_total() {
        let raw = RawConfig {
            widget_runtime_assets: Some(RawWidgetRuntimeAssets {
                store_path: None,
                max_total_bytes: Some(1024),
                max_agent_bytes: Some(2048),
            }),
            ..Default::default()
        };
        let err = resolve_runtime_widget_asset_store(&raw, None).unwrap_err();
        assert!(matches!(err.code, ConfigErrorCode::Other(_)));
        assert!(matches!(
            err.code,
            ConfigErrorCode::Other(ref code) if code == "CONFIG_WIDGET_ASSET_BUDGET_INVALID"
        ));
    }

    #[test]
    fn budget_validation_pushes_config_error() {
        let raw = RawConfig {
            widget_runtime_assets: Some(RawWidgetRuntimeAssets {
                store_path: None,
                max_total_bytes: Some(1),
                max_agent_bytes: Some(2),
            }),
            ..Default::default()
        };
        let mut errors = Vec::new();
        validate_runtime_widget_asset_budgets(&raw, &mut errors);
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            errors[0].code,
            ConfigErrorCode::Other(ref code) if code == "CONFIG_WIDGET_ASSET_BUDGET_INVALID"
        ));
    }

    #[test]
    fn relative_store_path_resolves_against_config_parent() {
        let base = tempdir().unwrap();
        let raw = RawConfig {
            widget_runtime_assets: Some(RawWidgetRuntimeAssets {
                store_path: Some("runtime_widget_assets".into()),
                max_total_bytes: None,
                max_agent_bytes: None,
            }),
            ..Default::default()
        };

        let resolved = resolve_store_path(&raw, Some(base.path()));
        assert_eq!(resolved, base.path().join("runtime_widget_assets"));
    }

    #[test]
    fn default_store_path_uses_cache_root_shape() {
        let raw = RawConfig::default();
        let path = resolve_store_path(&raw, None);
        let rendered = path.to_string_lossy();
        assert!(rendered.contains("tze_hud"));
        assert!(rendered.contains("resources"));
        assert!(rendered.contains(DEFAULT_STORE_DIRNAME));
    }

    #[test]
    fn explicit_store_path_is_created_and_writable() {
        let root = tempdir().unwrap();
        let target = root.path().join("new_widget_assets");
        let raw = RawConfig {
            widget_runtime_assets: Some(RawWidgetRuntimeAssets {
                store_path: Some(target.to_string_lossy().to_string()),
                max_total_bytes: None,
                max_agent_bytes: None,
            }),
            ..Default::default()
        };

        let resolved = resolve_runtime_widget_asset_store(&raw, None).unwrap();
        assert_eq!(resolved.store_path, target);
        assert!(resolved.store_path.exists());
    }
}
