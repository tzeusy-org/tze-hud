//! Per-user self-install, autostart, uninstall, and single instance (T6 step 2).
//!
//! Double-clicking the downloaded exe copies it to
//! `%LOCALAPPDATA%\Programs\tze_hud`, writes a default config under
//! `%APPDATA%\tze_hud` if none exists, registers a `HKCU` Run value and an
//! App Paths key, and relaunches the installed copy detached. `--uninstall
//! [--purge]` reverses it. A per-user named mutex keeps one instance running.
//!
//! No admin rights are used; only `tze_hud`-owned keys and directories are
//! touched. The decision, path, command-line, and file-staging logic is
//! platform independent (and unit tested on any host); the registry, mutex,
//! and event calls are `cfg(windows)`.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const APP_DIR_NAME: &str = "tze_hud";
pub const EXE_NAME: &str = "tze_hud.exe";
/// The previous exe, parked here while a running instance holds the lock on it.
pub const OLD_EXE_NAME: &str = "tze_hud.old.exe";
/// A new exe that failed its handoff and was renamed aside by a rollback.
pub const FAILED_EXE_NAME: &str = "tze_hud.failed.exe";
/// `HKCU` Run value name.
pub const RUN_VALUE_NAME: &str = "tze_hud";
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const APP_PATHS_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\App Paths\tze_hud.exe";
/// Per-user single-instance mutex.
pub const MUTEX_NAME: &str = "Local\\tze_hud";
/// Named event the uninstaller (and an upgrading installer) signals to ask the
/// running instance to shut down cleanly.
pub const QUIT_EVENT_NAME: &str = "Local\\tze_hud.quit";
/// How long `--handoff` waits for the previous instance to release the mutex.
pub const HANDOFF_WAIT: Duration = Duration::from_secs(35);

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("install and uninstall are Windows only")]
    WindowsOnly,
    #[error("environment variable {0} is not set")]
    MissingEnv(&'static str),
    #[error("refusing to touch {0}: not a tze_hud-owned directory")]
    NotOwned(PathBuf),
    #[error("path contains an unsupported character (\" or %): {0}")]
    UnsafePath(PathBuf),
    #[error("the running tze_hud did not exit within {0} s of the quit request; not replacing it")]
    StillRunning(u64),
    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: io::Error,
    },
}

fn io_err(context: &'static str) -> impl FnOnce(io::Error) -> InstallError {
    move |source| InstallError::Io { context, source }
}

// ── Paths ────────────────────────────────────────────────────────────────────

/// Every location install/uninstall reads or writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPaths {
    /// `%LOCALAPPDATA%\Programs\tze_hud`
    pub install_dir: PathBuf,
    /// `<install_dir>\tze_hud.exe`
    pub exe: PathBuf,
    /// `<install_dir>\tze_hud.old.exe`
    pub old_exe: PathBuf,
    /// `%APPDATA%\tze_hud` (config, agents.toml); kept by uninstall unless purged.
    pub config_dir: PathBuf,
    /// `<config_dir>\config.toml`
    pub config_file: PathBuf,
    /// `%LOCALAPPDATA%\tze_hud` (logs, element store); removed only by purge.
    pub data_dir: PathBuf,
}

impl InstallPaths {
    /// From the `LOCALAPPDATA` / `APPDATA` environment variables (CI redirects
    /// them to temp dirs).
    pub fn from_env() -> Result<Self, InstallError> {
        let var = |name: &'static str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .ok_or(InstallError::MissingEnv(name))
        };
        Ok(Self::from_dirs(&var("LOCALAPPDATA")?, &var("APPDATA")?))
    }

    pub fn from_dirs(local_app_data: &Path, roaming_app_data: &Path) -> Self {
        let install_dir = local_app_data.join("Programs").join(APP_DIR_NAME);
        let config_dir = roaming_app_data.join(APP_DIR_NAME);
        Self {
            exe: install_dir.join(EXE_NAME),
            old_exe: install_dir.join(OLD_EXE_NAME),
            config_file: config_dir.join("config.toml"),
            data_dir: local_app_data.join(APP_DIR_NAME),
            install_dir,
            config_dir,
        }
    }

    /// The command line autostart and the post-install relaunch both use.
    /// `<install_dir>\tze_hud.failed.exe`
    pub fn failed_exe(&self) -> PathBuf {
        self.install_dir.join(FAILED_EXE_NAME)
    }

    pub fn run_command(&self) -> String {
        format!(
            "\"{}\" --config \"{}\" --window-mode overlay",
            self.exe.display(),
            self.config_file.display()
        )
    }

    /// Arguments for the relaunch (the `run_command` minus the exe).
    pub fn launch_args(&self) -> Vec<OsString> {
        vec![
            "--config".into(),
            self.config_file.clone().into_os_string(),
            "--window-mode".into(),
            "overlay".into(),
        ]
    }
}

/// True for the launches that may exit silently with 0 when an instance is
/// already running: a bare launch, or exactly the autostart/relaunch command
/// (optionally with `--handoff`). Anything else carried explicit arguments
/// (benchmark, validation, CI) and must not no-op, so the caller exits non-zero.
pub fn is_canonical_launch(args: &[String], paths: &InstallPaths) -> bool {
    let rest: Vec<&String> = args.iter().filter(|a| *a != "--handoff").collect();
    if rest.is_empty() {
        return true;
    }
    matches!(
        rest.as_slice(),
        [c, cfg, m, mode]
            if *c == "--config"
                && same_path(Path::new(cfg), &paths.config_file)
                && *m == "--window-mode"
                && mode.eq_ignore_ascii_case("overlay")
    )
}

/// Windows path equality: case-insensitive, `/` and `\` alike, no `\\?\`
/// prefix, no trailing separator. Pure string comparison (no filesystem).
pub fn same_path(a: &Path, b: &Path) -> bool {
    fn norm(p: &Path) -> String {
        let s = p.to_string_lossy().replace('/', "\\");
        let s = s.strip_prefix("\\\\?\\").unwrap_or(&s);
        s.trim_end_matches('\\').to_lowercase()
    }
    norm(a) == norm(b)
}

// ── Decision ─────────────────────────────────────────────────────────────────

/// What the launch asked for, from the parsed CLI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Request {
    /// Any CLI argument at all was given.
    pub any_args: bool,
    pub install: bool,
    pub uninstall: bool,
    pub purge: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Run the HUD from where this exe is.
    Run,
    Install,
    Uninstall {
        purge: bool,
    },
}

/// Self-install only for a bare launch (no args) from outside the install
/// dir; any arg runs in place; `--install` forces it. `self_install_enabled`
/// is false off Windows so a bare dev launch never installs.
pub fn decide(
    req: Request,
    current_exe: &Path,
    paths: &InstallPaths,
    self_install_enabled: bool,
) -> Action {
    if req.uninstall {
        Action::Uninstall { purge: req.purge }
    } else if req.install
        || (!req.any_args && self_install_enabled && !same_path(current_exe, &paths.exe))
    {
        Action::Install
    } else {
        Action::Run
    }
}

// ── File staging (platform independent) ─────────────────────────────────────

/// Create `path` with `contents` unless it already exists. Returns whether it
/// was written; an existing file (the user's config) is never touched.
pub fn write_default_config(path: &Path, contents: &str) -> io::Result<bool> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => f.write_all(contents.as_bytes()).map(|()| true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Staged {
    pub exe_copied: bool,
    pub config_written: bool,
}

/// Copy the exe into the install dir and write the default config if absent.
/// An existing (possibly running, so locked) installed exe is renamed to
/// `tze_hud.old.exe` first, and restored if the copy fails. Idempotent.
pub fn stage_files(
    current_exe: &Path,
    paths: &InstallPaths,
    default_config: &str,
) -> Result<Staged, InstallError> {
    fs::create_dir_all(&paths.install_dir).map_err(io_err("create install dir"))?;
    let exe_copied = !same_path(current_exe, &paths.exe);
    if exe_copied {
        let had_old = paths.exe.exists();
        if had_old {
            fs::rename(&paths.exe, &paths.old_exe).map_err(io_err("park previous tze_hud.exe"))?;
        }
        if let Err(source) = fs::copy(current_exe, &paths.exe) {
            let _ = fs::remove_file(&paths.exe);
            if had_old {
                let _ = fs::rename(&paths.old_exe, &paths.exe);
            }
            return Err(InstallError::Io {
                context: "copy exe into install dir",
                source,
            });
        }
    }
    let config_written = write_default_config(&paths.config_file, default_config)
        .map_err(io_err("write default config"))?;
    Ok(Staged {
        exe_copied,
        config_written,
    })
}

/// Delete the exes parked by a previous upgrade or a rolled-back one. Best effort.
pub fn cleanup_old_exe(paths: &InstallPaths) {
    for path in [&paths.old_exe, &paths.failed_exe()] {
        if let Err(e) = fs::remove_file(path)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::debug!(error = %e, path = %path.display(), "could not remove a parked exe");
        }
    }
}

/// The `cmd /c` script that waits for this process to exit, then removes the
/// install dir (and, when purging, the config and data dirs). Only paths named
/// `tze_hud` are ever included.
pub fn cleanup_script(paths: &InstallPaths, purge: bool) -> Result<String, InstallError> {
    validate_owned(paths)?;
    let mut dirs = vec![&paths.install_dir];
    if purge {
        dirs.push(&paths.config_dir);
        dirs.push(&paths.data_dir);
    }
    // Let the exiting process get going, then retry each rmdir (about 1 s
    // apart, up to 30 times) until the locked exe is released; a dir that
    // survives is noted in uninstall.log next to the logs.
    let note = paths.data_dir.join("logs").join("uninstall.log");
    let mut script = String::from("ping -n 3 127.0.0.1>nul");
    for dir in dirs {
        let d = dir.to_string_lossy();
        script.push_str(&format!(
            " & (for /l %i in (1,1,30) do @if exist \"{d}\" (rmdir /s /q \"{d}\" 2>nul & ping -n 2 127.0.0.1>nul))"
        ));
        script.push_str(&format!(
            " & (if exist \"{d}\" echo could not remove {d} 1>>\"{}\" 2>nul)",
            note.to_string_lossy()
        ));
    }
    Ok(script)
}

/// Every directory install/uninstall may delete is `tze_hud`-named, and no
/// path holds a character the cleanup script cannot quote. Checked at install
/// too, so an install is never created that uninstall would refuse.
pub fn validate_owned(paths: &InstallPaths) -> Result<(), InstallError> {
    for dir in [&paths.install_dir, &paths.config_dir, &paths.data_dir] {
        if dir.file_name().is_none_or(|n| n != APP_DIR_NAME) || dir.components().count() < 3 {
            return Err(InstallError::NotOwned(dir.clone()));
        }
    }
    for path in [
        &paths.install_dir,
        &paths.config_dir,
        &paths.data_dir,
        &paths.exe,
        &paths.config_file,
    ] {
        let text = path.to_string_lossy();
        if text.contains('"') || text.contains('%') {
            return Err(InstallError::UnsafePath(path.clone()));
        }
    }
    Ok(())
}

// ── Orchestration ────────────────────────────────────────────────────────────

/// Install this exe per user, register autostart, and relaunch detached.
/// Replaces (upgrades) a running installed instance.
pub fn install(
    current_exe: &Path,
    paths: &InstallPaths,
    default_config: &str,
) -> Result<Staged, InstallError> {
    #[cfg(windows)]
    {
        validate_owned(paths)?;
        const QUIT_WAIT: Duration = Duration::from_secs(15);
        if win::signal_quit() && !win::wait_for_instance_exit(QUIT_WAIT) {
            return Err(InstallError::StillRunning(QUIT_WAIT.as_secs()));
        }
        let staged = stage_files(current_exe, paths, default_config)?;
        win::register(paths).map_err(io_err("register autostart"))?;
        // --handoff: if anything still holds the instance mutex, wait for it.
        let mut args = paths.launch_args();
        args.push("--handoff".into());
        win::spawn_detached(&paths.exe, &args).map_err(io_err("relaunch"))?;
        Ok(staged)
    }
    #[cfg(not(windows))]
    {
        let _ = (current_exe, paths, default_config);
        Err(InstallError::WindowsOnly)
    }
}

/// Remove autostart, stop the running instance, and delete the install dir
/// (and, with `purge`, the config and data dirs) once this process is gone.
pub fn uninstall(paths: &InstallPaths, purge: bool) -> Result<(), InstallError> {
    #[cfg(windows)]
    {
        // Validate before changing anything.
        let script = cleanup_script(paths, purge)?;
        win::unregister().map_err(io_err("remove autostart"))?;
        win::signal_quit();
        win::spawn_cleanup(&script).map_err(io_err("schedule cleanup"))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (paths, purge);
        Err(InstallError::WindowsOnly)
    }
}

// ── Single instance + quit signal ───────────────────────────────────────────

/// Held for the process lifetime; releases the instance mutex on drop.
pub struct InstanceGuard {
    #[cfg(windows)]
    handle: usize,
}

#[cfg(windows)]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        win::close(self.handle);
    }
}

pub enum Acquire {
    Acquired(InstanceGuard),
    AlreadyRunning,
}

/// Take the per-user instance mutex, waiting up to `wait` for a previous
/// holder to release it (`--handoff`). Always acquires off Windows.
pub fn acquire_single_instance(wait: Duration) -> Acquire {
    #[cfg(windows)]
    {
        win::acquire(wait)
    }
    #[cfg(not(windows))]
    {
        let _ = wait;
        Acquire::Acquired(InstanceGuard {})
    }
}

/// Run `on_quit` once if another process signals the quit event. No-op off
/// Windows.
pub fn spawn_quit_listener(on_quit: impl FnOnce() + Send + 'static) {
    #[cfg(windows)]
    win::spawn_quit_listener(on_quit);
    #[cfg(not(windows))]
    let _ = on_quit;
}

#[cfg(windows)]
mod win {
    use super::{
        APP_PATHS_KEY, Acquire, InstallPaths, InstanceGuard, MUTEX_NAME, QUIT_EVENT_NAME, RUN_KEY,
        RUN_VALUE_NAME,
    };
    use std::ffi::{OsStr, OsString, c_void};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
        GetLastError, HANDLE, WAIT_OBJECT_0, WIN32_ERROR,
    };
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteKeyW, RegSetValueExW,
    };
    use windows::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CreateEventW, CreateMutexW, DETACHED_PROCESS, EVENT_MODIFY_STATE,
        INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
    };
    use windows::core::PCWSTR;

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    fn check(code: WIN32_ERROR, what: &str) -> io::Result<()> {
        if code.0 == 0 {
            Ok(())
        } else {
            let os = io::Error::from_raw_os_error(code.0 as i32);
            Err(io::Error::new(os.kind(), format!("{what}: {os}")))
        }
    }

    fn is_missing(code: WIN32_ERROR) -> bool {
        code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND
    }

    pub(super) fn close(handle: usize) {
        // SAFETY: `handle` came from a successful Create*/Open* call and is
        // closed exactly once by its owner.
        unsafe {
            let _ = CloseHandle(HANDLE(handle as *mut c_void));
        }
    }

    /// Write a `REG_SZ` under `HKCU\<subkey>`; `name` None is the default value.
    fn set_string(subkey: &str, name: Option<&str>, value: &str) -> io::Result<()> {
        let subkey = wide(subkey);
        let name = name.map(wide);
        let data: Vec<u8> = wide(value).iter().flat_map(|u| u.to_le_bytes()).collect();
        let mut key = HKEY::default();
        // SAFETY: all pointers are to live, NUL-terminated buffers; `key` is
        // written by the call and closed below.
        unsafe {
            check(
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    PCWSTR(subkey.as_ptr()),
                    0,
                    PCWSTR::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_SET_VALUE,
                    None,
                    &mut key,
                    None,
                ),
                "RegCreateKeyExW",
            )?;
            let name_ptr = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
            let result = check(
                RegSetValueExW(key, name_ptr, 0, REG_SZ, Some(&data)),
                "RegSetValueExW",
            );
            let _ = RegCloseKey(key);
            result
        }
    }

    pub(super) fn register(paths: &InstallPaths) -> io::Result<()> {
        set_string(RUN_KEY, Some(RUN_VALUE_NAME), &paths.run_command())?;
        set_string(APP_PATHS_KEY, None, &paths.exe.to_string_lossy())?;
        set_string(
            APP_PATHS_KEY,
            Some("Path"),
            &paths.install_dir.to_string_lossy(),
        )
    }

    pub(super) fn unregister() -> io::Result<()> {
        let run = wide(RUN_KEY);
        let value = wide(RUN_VALUE_NAME);
        let app_paths = wide(APP_PATHS_KEY);
        // SAFETY: pointers are to live, NUL-terminated buffers.
        unsafe {
            let code = RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(run.as_ptr()),
                PCWSTR(value.as_ptr()),
            );
            if !is_missing(code) {
                check(code, "RegDeleteKeyValueW")?;
            }
            let code = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(app_paths.as_ptr()));
            if !is_missing(code) {
                check(code, "RegDeleteKeyW")?;
            }
        }
        Ok(())
    }

    /// Detached, console-less child that outlives this process.
    fn detached(program: &OsStr) -> std::process::Command {
        let mut cmd = std::process::Command::new(program);
        cmd.creation_flags((CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS).0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd
    }

    pub(super) fn spawn_detached(exe: &Path, args: &[OsString]) -> io::Result<()> {
        detached(exe.as_os_str()).args(args).spawn().map(|_| ())
    }

    pub(super) fn spawn_cleanup(script: &str) -> io::Result<()> {
        // `/s` strips the outer quotes, leaving the inner ones intact.
        detached(OsStr::new("cmd"))
            .raw_arg(format!("/d /s /c \"{script}\""))
            .spawn()
            .map(|_| ())
    }

    /// Ask a running instance to shut down. True if one was listening.
    pub(super) fn signal_quit() -> bool {
        let name = wide(QUIT_EVENT_NAME);
        // SAFETY: `name` is a live NUL-terminated buffer; the handle is closed.
        unsafe {
            let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) else {
                return false;
            };
            let signalled = SetEvent(event).is_ok();
            let _ = CloseHandle(event);
            signalled
        }
    }

    /// True once no instance holds the mutex (within `timeout`).
    pub(super) fn wait_for_instance_exit(timeout: Duration) -> bool {
        matches!(acquire(timeout), Acquire::Acquired(_))
    }

    pub(super) fn acquire(wait: Duration) -> Acquire {
        let name = wide(MUTEX_NAME);
        let deadline = Instant::now() + wait;
        loop {
            // SAFETY: `name` is a live NUL-terminated buffer. GetLastError is
            // read immediately after the call that sets it.
            let (handle, exists) = unsafe {
                match CreateMutexW(None, false, PCWSTR(name.as_ptr())) {
                    Ok(h) => (h, GetLastError() == ERROR_ALREADY_EXISTS),
                    Err(e) => {
                        // Fail open: a broken mutex must not stop the HUD.
                        tracing::warn!(error = %e, "single-instance mutex unavailable; continuing");
                        return Acquire::Acquired(InstanceGuard { handle: 0 });
                    }
                }
            };
            if !exists {
                return Acquire::Acquired(InstanceGuard {
                    handle: handle.0 as usize,
                });
            }
            close(handle.0 as usize);
            if Instant::now() >= deadline {
                return Acquire::AlreadyRunning;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    pub(super) fn spawn_quit_listener(on_quit: impl FnOnce() + Send + 'static) {
        let name = wide(QUIT_EVENT_NAME);
        // SAFETY: `name` is a live NUL-terminated buffer.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) };
        let event = match event {
            Ok(e) => e.0 as usize,
            Err(e) => {
                tracing::warn!(error = %e, "quit event unavailable; remote quit disabled");
                return;
            }
        };
        let spawned = std::thread::Builder::new()
            .name("quit-listener".into())
            .spawn(move || {
                // SAFETY: the event handle lives for the process lifetime.
                let waited = unsafe { WaitForSingleObject(HANDLE(event as *mut c_void), INFINITE) };
                if waited == WAIT_OBJECT_0 {
                    tracing::info!("quit event signalled; shutting down");
                    on_quit();
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start quit listener");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> InstallPaths {
        InstallPaths::from_dirs(Path::new("/u/Local"), Path::new("/u/Roaming"))
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tze_hud_install_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn decision_table() {
        let p = paths();
        let outside = Path::new("/dl/tze_hud.exe");
        let args = |any_args, install, uninstall, purge| Request {
            any_args,
            install,
            uninstall,
            purge,
        };
        let cases = [
            // (request, current exe, self-install enabled, expected)
            (
                args(false, false, false, false),
                outside,
                true,
                Action::Install,
            ),
            (args(false, false, false, false), &p.exe, true, Action::Run),
            (
                args(false, false, false, false),
                outside,
                false,
                Action::Run,
            ),
            (args(true, false, false, false), outside, true, Action::Run),
            (
                args(true, true, false, false),
                outside,
                true,
                Action::Install,
            ),
            (
                args(true, true, false, false),
                &p.exe,
                false,
                Action::Install,
            ),
            (
                args(true, false, true, false),
                &p.exe,
                true,
                Action::Uninstall { purge: false },
            ),
            (
                args(true, false, true, true),
                outside,
                true,
                Action::Uninstall { purge: true },
            ),
        ];
        for (req, exe, enabled, want) in cases {
            assert_eq!(decide(req, exe, &p, enabled), want, "{req:?} from {exe:?}");
        }
    }

    #[test]
    fn same_path_ignores_case_separators_and_verbatim_prefix() {
        assert!(same_path(
            Path::new(r"C:\Users\A\AppData\Local\Programs\tze_hud\tze_hud.exe"),
            Path::new("c:/users/a/appdata/local/programs/tze_hud/TZE_HUD.EXE"),
        ));
        assert!(same_path(Path::new(r"\\?\C:\x\y\"), Path::new(r"C:\x\y")));
        assert!(!same_path(Path::new(r"C:\x\y"), Path::new(r"C:\x\z")));
    }

    #[test]
    fn install_paths_from_fake_env_dirs() {
        let p = InstallPaths::from_dirs(Path::new("/L"), Path::new("/R"));
        assert_eq!(p.install_dir, Path::new("/L/Programs/tze_hud"));
        assert_eq!(p.exe, Path::new("/L/Programs/tze_hud/tze_hud.exe"));
        assert_eq!(p.old_exe, Path::new("/L/Programs/tze_hud/tze_hud.old.exe"));
        assert_eq!(p.config_file, Path::new("/R/tze_hud/config.toml"));
        assert_eq!(p.data_dir, Path::new("/L/tze_hud"));
    }

    #[test]
    fn run_command_and_launch_args_agree() {
        let p = paths();
        assert_eq!(
            p.run_command(),
            format!(
                "\"{}\" --config \"{}\" --window-mode overlay",
                p.exe.display(),
                p.config_file.display()
            )
        );
        assert_eq!(p.launch_args()[1], p.config_file.as_os_str());
    }

    #[test]
    fn default_config_is_not_overwritten() {
        let dir = scratch("cfg");
        let file = dir.join("nested").join("config.toml");
        assert!(write_default_config(&file, "first").unwrap());
        assert!(!write_default_config(&file, "second").unwrap());
        assert_eq!(fs::read_to_string(&file).unwrap(), "first");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stage_files_installs_upgrades_and_is_idempotent() {
        let dir = scratch("stage");
        let p = InstallPaths::from_dirs(&dir.join("L"), &dir.join("R"));
        let src = dir.join("download.exe");
        fs::write(&src, "v1").unwrap();

        let first = stage_files(&src, &p, "cfg").unwrap();
        assert_eq!(
            first,
            Staged {
                exe_copied: true,
                config_written: true
            }
        );
        assert_eq!(fs::read_to_string(&p.exe).unwrap(), "v1");

        // Upgrade: previous exe is parked, user config is kept.
        fs::write(&src, "v2").unwrap();
        fs::write(&p.config_file, "edited").unwrap();
        let second = stage_files(&src, &p, "cfg").unwrap();
        assert_eq!(
            second,
            Staged {
                exe_copied: true,
                config_written: false
            }
        );
        assert_eq!(fs::read_to_string(&p.exe).unwrap(), "v2");
        assert_eq!(fs::read_to_string(&p.old_exe).unwrap(), "v1");
        assert_eq!(fs::read_to_string(&p.config_file).unwrap(), "edited");

        // Re-running from the installed exe copies nothing.
        let third = stage_files(&p.exe, &p, "cfg").unwrap();
        assert!(!third.exe_copied);
        cleanup_old_exe(&p);
        assert!(!p.old_exe.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stage_files_restores_previous_exe_when_copy_fails() {
        let dir = scratch("restore");
        let p = InstallPaths::from_dirs(&dir.join("L"), &dir.join("R"));
        fs::create_dir_all(&p.install_dir).unwrap();
        fs::write(&p.exe, "installed").unwrap();
        assert!(stage_files(&dir.join("missing.exe"), &p, "cfg").is_err());
        assert_eq!(fs::read_to_string(&p.exe).unwrap(), "installed");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cleanup_script_scopes_to_owned_dirs() {
        let p = paths();
        let keep = cleanup_script(&p, false).unwrap();
        assert!(keep.contains(&p.install_dir.to_string_lossy().into_owned()));
        assert!(
            !keep.contains("Roaming"),
            "config kept without --purge: {keep}"
        );
        let purge = cleanup_script(&p, true).unwrap();
        assert_eq!(purge.matches("rmdir").count(), 3);
        assert!(purge.contains("for /l"), "rmdir is retried: {purge}");

        let mut bad = paths();
        bad.install_dir = PathBuf::from("/u/Local/Programs");
        assert!(matches!(
            cleanup_script(&bad, false),
            Err(InstallError::NotOwned(_))
        ));
        let mut pct = paths();
        pct.install_dir = PathBuf::from("/u/%TEMP%/tze_hud");
        assert!(matches!(
            cleanup_script(&pct, false),
            Err(InstallError::UnsafePath(_))
        ));
    }

    #[test]
    fn validate_owned_rejects_what_cleanup_cannot_quote() {
        assert!(validate_owned(&paths()).is_ok());
        let mut p = paths();
        p.config_file = PathBuf::from("/u/Roaming/tze_hud/a\"b.toml");
        assert!(matches!(
            validate_owned(&p),
            Err(InstallError::UnsafePath(_))
        ));
        let mut p = paths();
        p.data_dir = PathBuf::from("/u/%X%/tze_hud");
        assert!(matches!(
            validate_owned(&p),
            Err(InstallError::UnsafePath(_))
        ));
    }

    #[test]
    fn canonical_launch_is_bare_or_the_autostart_command() {
        let p = paths();
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let cfg = p.config_file.to_string_lossy().into_owned();
        assert!(is_canonical_launch(&[], &p));
        assert!(is_canonical_launch(&a(&["--handoff"]), &p));
        assert!(is_canonical_launch(
            &a(&["--config", &cfg, "--window-mode", "overlay", "--handoff"]),
            &p
        ));
        assert!(!is_canonical_launch(&a(&["--mcp-port", "9091"]), &p));
        assert!(!is_canonical_launch(
            &a(&["--config", "/other.toml", "--window-mode", "overlay"]),
            &p
        ));
        assert!(!is_canonical_launch(
            &a(&["--config", &cfg, "--window-mode", "fullscreen"]),
            &p
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn install_and_uninstall_are_windows_only_here() {
        let p = paths();
        assert!(matches!(
            install(Path::new("x"), &p, ""),
            Err(InstallError::WindowsOnly)
        ));
        assert!(matches!(
            uninstall(&p, false),
            Err(InstallError::WindowsOnly)
        ));
    }
}
