//! Configuration file path resolution.
//!
//! Implements spec §Requirement: Configuration File Resolution Order:
//!
//! 1. `--config <path>` CLI flag
//! 2. `$TZE_HUD_CONFIG` environment variable
//! 3. `./tze_hud.toml` in the current working directory
//! 4. `$XDG_CONFIG_HOME/tze_hud/config.toml` (Linux/macOS)
//! 5. `%APPDATA%\tze_hud\config.toml` (Windows)
//!
//! Returns the **first** path that exists as a file, or an error listing all
//! searched paths.

use std::path::PathBuf;

/// Resolve the configuration file path according to the search chain.
///
/// `cli_path`: value of the `--config` CLI flag, if provided.
///
/// Returns `Ok(path_string)` for the first path found as an existing file,
/// or `Err(searched_paths)` if no file is found.
pub fn resolve_config_path(cli_path: Option<&str>) -> Result<String, Vec<String>> {
    let mut searched: Vec<String> = Vec::new();

    // 1. CLI flag
    if let Some(p) = cli_path {
        let path = PathBuf::from(p);
        let s = path.to_string_lossy().to_string();
        if path.is_file() {
            return Ok(s);
        }
        searched.push(s);
        // CLI path was explicitly given but not found — return immediately with
        // only that path. When --config is explicit, no other locations are tried.
        return Err(searched);
    }

    // 2. Environment variable
    if let Ok(env_val) = std::env::var("TZE_HUD_CONFIG") {
        let path = PathBuf::from(&env_val);
        let s = path.to_string_lossy().to_string();
        if path.is_file() {
            return Ok(s);
        }
        searched.push(s);
    }

    // 3. Current working directory
    {
        let path = PathBuf::from("tze_hud.toml");
        let s = path.to_string_lossy().to_string();
        if path.is_file() {
            return Ok(s);
        }
        searched.push(s);
    }

    // 4. XDG / platform config dir
    if let Some(config_dir) = xdg_config_home() {
        let path = config_dir.join("tze_hud").join("config.toml");
        let s = path.to_string_lossy().to_string();
        if path.is_file() {
            return Ok(s);
        }
        searched.push(s);
    }

    Err(searched)
}

/// Returns the platform config home directory.
///
/// - Linux/macOS: `$XDG_CONFIG_HOME` if set, else `$HOME/.config`
/// - Windows: `%APPDATA%`
pub(crate) fn xdg_config_home() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA").ok().map(PathBuf::from)
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(xdg));
        }
        std::env::var("HOME")
            .ok()
            .map(|home| PathBuf::from(home).join(".config"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WHEN no config file exists at any path THEN Err with all searched paths.
    #[test]
    fn test_resolve_returns_error_with_searched_paths_when_not_found() {
        // Use a non-existent explicit path so we get deterministic behaviour.
        let result = resolve_config_path(Some("/tmp/tze_hud_nonexistent_test_file_j90m.toml"));
        match result {
            Err(paths) => {
                assert!(!paths.is_empty(), "searched paths must be non-empty");
                assert!(
                    paths[0].contains("tze_hud_nonexistent_test_file_j90m"),
                    "searched paths should include the CLI path"
                );
            }
            Ok(_) => panic!("should not find a non-existent file"),
        }
    }

    /// WHEN a file exists at the given CLI path THEN it is returned.
    #[test]
    fn test_resolve_cli_path_found() {
        // Create a temp file.
        let dir = std::env::temp_dir();
        let file = dir.join("tze_hud_test_j90m_cli.toml");
        std::fs::write(
            &file,
            b"[runtime]\nprofile = \"headless\"\n[[tabs]]\nname = \"T\"\n",
        )
        .unwrap();
        let result = resolve_config_path(Some(file.to_str().unwrap()));
        assert!(result.is_ok(), "should find the file");
        assert_eq!(result.unwrap(), file.to_string_lossy().as_ref());
        let _ = std::fs::remove_file(&file);
    }
}
