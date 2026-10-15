//! What the Network, Diagnostics and System sections act on: the launch
//! settings and their sources, the config file, restart and update, the
//! firewall helper, autostart, and opening files. Every outside effect is a
//! field, so tests swap in fakes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tze_hud_config::RuntimeUpdate;

use crate::firewall::FirewallProbe;
use crate::firewall::remote::RemoteAction;
use crate::launch::{LaunchSources, LiveSettings};
use crate::operator::handoff::RestartHandle;
use crate::operator::install::InstallPaths;
use crate::operator::update::UpdateHandle;

/// Writes `[runtime]` keys to the config file.
pub(crate) type ConfigWriter = Arc<dyn Fn(&[RuntimeUpdate]) -> Result<(), String> + Send + Sync>;
/// Runs the elevated firewall helper for the MCP and gRPC ports.
pub(crate) type RemoteFirewall =
    Arc<dyn Fn(RemoteAction, [u16; 2]) -> Result<String, String> + Send + Sync>;
/// Reads (`None`) or sets (`Some`) start-at-sign-in; returns the state after.
pub(crate) type Autostart = Arc<dyn Fn(Option<bool>) -> Result<bool, String> + Send + Sync>;
/// Saves one `[[tabs.widgets]]` entry: (tab, instance, change).
pub(crate) type WidgetWriter =
    Arc<dyn Fn(&str, &str, &tze_hud_config::WidgetUpdate) -> Result<(), String> + Send + Sync>;
/// Sets the log file's level.
pub(crate) type LevelSetter = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
/// The log file's level: read (`None`: no log file) and set.
#[derive(Clone)]
pub(crate) struct LogLevel {
    pub get: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    pub set: LevelSetter,
}
/// Opens a file or folder the way Explorer would.
pub(crate) type Opener = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// The ports a restart would use, as saved; compared with the running ones
/// to show what a restart applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ports {
    pub mcp: u16,
    pub grpc: u16,
}

#[derive(Clone)]
pub(crate) struct RuntimeControl {
    /// The config file the panel saves to (`None`: nothing is saved).
    pub config_path: Option<PathBuf>,
    /// Where each value came from; a panel save makes it `Config`.
    pub sources: Arc<Mutex<LaunchSources>>,
    /// The ports this instance listens on.
    pub running: Ports,
    /// The ports the next start will use.
    pub saved: Arc<Mutex<Ports>>,
    pub live: Arc<LiveSettings>,
    pub write: ConfigWriter,
    /// `None` when there is no config file to save widgets to.
    pub write_widget: Option<WidgetWriter>,
    /// `[displays.<NAME>]` zone lists, sorted by display.
    pub displays: Vec<(String, Vec<String>)>,
    pub restart: Option<RestartHandle>,
    pub update: Option<UpdateHandle>,
    /// The channel Install update pulls from.
    pub channel: String,
    /// Windows Firewall's verdict for tailnet agents (`None`: no MCP port).
    pub firewall: Option<Arc<FirewallProbe>>,
    pub remote: RemoteFirewall,
    /// `None` when this copy is not installed (nothing to start at sign-in).
    pub autostart: Option<Autostart>,
    pub open: Opener,
    pub log_dir: PathBuf,
    pub log_level: LogLevel,
}

impl RuntimeControl {
    /// Production wiring for the HUD that `cfg` started.
    pub(crate) fn system(
        cfg: &crate::windowed::WindowedConfig,
        live: Arc<LiveSettings>,
        restart: Option<RestartHandle>,
        update: Option<UpdateHandle>,
        firewall: Option<Arc<FirewallProbe>>,
    ) -> Self {
        let config_path = cfg.config_file_path.clone().map(PathBuf::from);
        let sources = cfg.sources;
        let running = Ports {
            mcp: cfg.mcp_port,
            grpc: cfg.grpc_port,
        };
        // The next start reads what config holds unless a flag or variable
        // pins it.
        let config = cfg
            .config_toml
            .as_deref()
            .and_then(tze_hud_config::runtime_section)
            .unwrap_or_default();
        let saved = Ports {
            mcp: config
                .mcp_port
                .filter(|_| !sources.mcp_port.is_pinned())
                .unwrap_or(running.mcp),
            grpc: config
                .grpc_port
                .filter(|_| !sources.grpc_port.is_pinned())
                .unwrap_or(running.grpc),
        };
        let write: ConfigWriter = match config_path.clone() {
            Some(path) => Arc::new(move |updates: &[RuntimeUpdate]| {
                tze_hud_config::set_runtime(&path, updates).map_err(|e| e.to_string())
            }),
            None => Arc::new(|_: &[RuntimeUpdate]| Err("no config file to save to".to_owned())),
        };
        // Only the installed copy updates itself or starts at sign-in: from
        // a dev or portable copy these would act on (or register) whatever
        // sits in the install folder instead.
        let installed = installed(
            std::env::current_exe().ok().as_deref(),
            InstallPaths::from_env().ok(),
        );
        let update = update.filter(|_| installed.is_some());
        let autostart = installed.filter(|_| cfg!(windows)).map(|paths| {
            Arc::new(move |set: Option<bool>| {
                if let Some(on) = set {
                    crate::operator::install::set_autostart(&paths, on)
                        .map_err(|e| e.to_string())?;
                }
                crate::operator::install::autostart_enabled().map_err(|e| e.to_string())
            }) as Autostart
        });
        let write_widget = config_path.clone().map(|path| {
            Arc::new(
                move |tab: &str, instance: &str, update: &tze_hud_config::WidgetUpdate| {
                    tze_hud_config::set_widget(&path, tab, instance, update)
                        .map_err(|e| e.to_string())
                },
            ) as WidgetWriter
        });
        let mut displays: Vec<(String, Vec<String>)> = cfg
            .config_toml
            .as_deref()
            .and_then(|src| toml::from_str::<tze_hud_config::raw::RawConfig>(src).ok())
            .and_then(|raw| raw.displays)
            .into_iter()
            .flatten()
            .map(|(name, table)| (name, table.zones))
            .collect();
        displays.sort();
        Self {
            config_path,
            write_widget,
            displays,
            sources: Arc::new(Mutex::new(sources)),
            running,
            saved: Arc::new(Mutex::new(saved)),
            live,
            write,
            restart,
            update,
            channel: crate::operator::status::build_channel(),
            firewall,
            remote: Arc::new(crate::firewall::remote::execute),
            autostart,
            open: Arc::new(open_in_explorer),
            log_dir: crate::operator::logs::log_dir(),
            log_level: LogLevel {
                get: Arc::new(crate::operator::logs::file_level),
                set: Arc::new(crate::operator::logs::set_file_level),
            },
        }
    }

    pub(crate) fn sources(&self) -> LaunchSources {
        *self.sources.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Save `update` to the config, recording that its value now comes from
    /// there.
    pub(crate) fn save(
        &self,
        update: RuntimeUpdate,
        source: impl FnOnce(&mut LaunchSources) -> &mut crate::launch::Source,
    ) -> Result<(), String> {
        (self.write)(std::slice::from_ref(&update))?;
        *source(&mut self.sources.lock().unwrap_or_else(|e| e.into_inner())) =
            crate::launch::Source::Config;
        Ok(())
    }

    pub(crate) fn saved(&self) -> Ports {
        *self.saved.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What a restart would apply, by row label.
    pub(crate) fn pending(&self) -> Vec<&'static str> {
        let saved = self.saved();
        let mut pending = Vec::new();
        if saved.mcp != self.running.mcp {
            pending.push("MCP port");
        }
        if saved.grpc != self.running.grpc {
            pending.push("gRPC port");
        }
        pending
    }
}

/// Open `path` with its default app (a folder in Explorer).
fn open_in_explorer(path: &Path) -> Result<(), String> {
    if !cfg!(windows) {
        return Err("Opens on Windows only".to_owned());
    }
    // Explorer returns a nonzero code even on success, so only a failed
    // spawn is an error.
    std::process::Command::new("explorer")
        .arg(path)
        .spawn()
        .map(drop)
        .map_err(|e| format!("could not start Explorer: {e}"))
}

/// The install layout, when `exe` is the installed copy.
fn installed(exe: Option<&Path>, paths: Option<InstallPaths>) -> Option<InstallPaths> {
    paths.filter(|p| exe.is_some_and(|exe| crate::operator::install::same_path(exe, &p.exe)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A restart applies a saved port unless a flag or variable pins the
    /// running one (the restart reuses it).
    #[test]
    fn a_pinned_port_has_nothing_to_apply() {
        let cfg = |pinned: bool| crate::windowed::WindowedConfig {
            config_toml: Some("[runtime]\nprofile = \"full-display\"\nmcp_port = 9290\n".into()),
            mcp_port: 9191,
            grpc_port: 50051,
            sources: LaunchSources {
                mcp_port: if pinned {
                    crate::launch::Source::Env("TZE_HUD_MCP_PORT")
                } else {
                    crate::launch::Source::Config
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let live = || Arc::new(LiveSettings::new(60, false, Default::default()));
        let control = RuntimeControl::system(&cfg(true), live(), None, None, None);
        assert!(control.pending().is_empty());
        let control = RuntimeControl::system(&cfg(false), live(), None, None, None);
        assert_eq!(control.pending(), ["MCP port"]);
    }

    /// Update and autostart are offered only by the installed copy.
    #[test]
    fn only_the_installed_copy_counts_as_installed() {
        let paths = InstallPaths::from_dirs(Path::new(r"C:\L"), Path::new(r"C:\R"));
        let exe = paths.exe.clone();
        assert_eq!(
            installed(Some(&exe), Some(paths.clone())),
            Some(paths.clone())
        );
        assert_eq!(
            installed(Some(Path::new(r"C:\dev\tze_hud.exe")), Some(paths.clone())),
            None
        );
        assert_eq!(installed(None, Some(paths)), None);
        assert_eq!(installed(Some(&exe), None), None);
    }

    /// A control saving to `config` (created with a `[runtime]` table),
    /// running on 9090/50051, whose outside effects are pushed to `effects`.
    pub(crate) fn fake(
        config: &Path,
        effects: Arc<Mutex<Vec<String>>>,
        firewall: Option<Arc<FirewallProbe>>,
    ) -> RuntimeControl {
        let toml = "[runtime]\nprofile = \"full-display\"\n";
        std::fs::write(config, toml).unwrap();
        let cfg = crate::windowed::WindowedConfig {
            config_file_path: Some(config.to_string_lossy().into_owned()),
            config_toml: Some(toml.to_owned()),
            mcp_port: 9090,
            grpc_port: 50051,
            ..Default::default()
        };
        let mut control = RuntimeControl::system(
            &cfg,
            Arc::new(LiveSettings::new(60, false, Default::default())),
            None,
            None,
            firewall,
        );
        let remote_effects = Arc::clone(&effects);
        control.remote = Arc::new(move |action, ports| {
            remote_effects
                .lock()
                .unwrap()
                .push(format!("{action:?} {ports:?}"));
            Ok(String::new())
        });
        control.open = Arc::new(move |path| {
            effects
                .lock()
                .unwrap()
                .push(format!("open {}", path.display()));
            Ok(())
        });
        control.autostart = None;
        control
    }
}
