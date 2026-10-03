//! `GET /admin/status`: one JSON object describing the running HUD.
//!
//! Carries no secrets: agents appear by id with an `admin` flag, never by PSK
//! or hash.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tze_hud_scene::config::SharedAgents;

use crate::idle_efficiency::IdleEfficiencyCounters;

/// Build identity, registered once by the binary at startup.
#[derive(Debug, Clone)]
pub struct BuildInfo {
    pub sha: String,
    pub channel: String,
}

/// Outcome of registering the human safe-mode override chord.
///
/// Platform-neutral so the banner, `/admin/status`, and tests need no Windows
/// types. Only the Windows hotkey thread produces `Registered` / `Failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyStatus {
    /// No global hotkey on this platform (or it was never started).
    NotApplicable,
    Registered {
        chord: String,
    },
    /// Registration has not reported yet; the outcome is unknown, not a failure.
    Pending {
        chord: String,
    },
    /// The chord could not be registered (typically another program owns it),
    /// so the human override is unavailable.
    Failed {
        chord: String,
        reason: String,
    },
}

impl HotkeyStatus {
    /// One banner line, or `None` when there is nothing to say.
    pub fn banner_line(&self) -> Option<String> {
        match self {
            Self::NotApplicable => None,
            Self::Registered { chord } => Some(format!("   safe   : {chord} toggles safe mode")),
            Self::Pending { chord } => Some(format!(
                "   safe   : hotkey {chord} registration pending (see /admin/status)"
            )),
            Self::Failed { chord, reason } => Some(format!(
                "   safe   : WARNING hotkey {chord} NOT registered ({reason}); no human safe-mode override"
            )),
        }
    }

    /// The `safe_mode_hotkey` value for `/admin/status`: `null` when not applicable.
    pub fn to_json(&self) -> Value {
        match self {
            Self::NotApplicable => Value::Null,
            Self::Registered { chord } => {
                json!({"chord": chord, "registered": true, "error": Value::Null})
            }
            Self::Pending { chord } => {
                json!({"chord": chord, "registered": Value::Null, "error": Value::Null})
            }
            Self::Failed { chord, reason } => {
                json!({"chord": chord, "registered": false, "error": reason})
            }
        }
    }
}

static SAFE_MODE_HOTKEY: Mutex<HotkeyStatus> = Mutex::new(HotkeyStatus::NotApplicable);

/// Record the hotkey outcome and log it. Updateable: the hotkey thread may
/// report after the starter gave up waiting. A failure is logged at error
/// level because the human has no other override. A `Pending` never replaces
/// a definitive outcome (the thread can win the race against the starter).
pub fn set_safe_mode_hotkey(status: HotkeyStatus) -> HotkeyStatus {
    let mut cur = SAFE_MODE_HOTKEY.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(status, HotkeyStatus::Pending { .. })
        && matches!(
            *cur,
            HotkeyStatus::Registered { .. } | HotkeyStatus::Failed { .. }
        )
    {
        return cur.clone();
    }
    match &status {
        HotkeyStatus::Failed { chord, reason } => {
            tracing::error!(%chord, %reason, "safe-mode hotkey NOT registered: no human safe-mode override");
        }
        HotkeyStatus::Pending { chord } => {
            tracing::warn!(%chord, "safe-mode hotkey registration pending");
        }
        HotkeyStatus::Registered { chord } => {
            tracing::info!(%chord, "safe-mode global hotkey registered");
        }
        HotkeyStatus::NotApplicable => {}
    }
    *cur = status.clone();
    status
}

/// Hand a hotkey outcome to the starter waiting on `tx`. If the starter has
/// already stopped waiting (timed out with `Pending`), record it directly so
/// the late result supersedes `Pending`.
pub fn report_outcome(tx: &std::sync::mpsc::Sender<HotkeyStatus>, status: HotkeyStatus) {
    if let Err(std::sync::mpsc::SendError(status)) = tx.send(status) {
        set_safe_mode_hotkey(status);
    }
}

/// The recorded hotkey outcome, `NotApplicable` until one is set.
pub fn safe_mode_hotkey() -> HotkeyStatus {
    SAFE_MODE_HOTKEY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

static BUILD_INFO: OnceLock<BuildInfo> = OnceLock::new();
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Record the build identity (first call wins) and the process start time.
pub fn set_build_info(info: BuildInfo) {
    process_start();
    let _ = BUILD_INFO.set(info);
}

/// Full git SHA of this build, `"unknown"` before [`set_build_info`].
pub fn build_sha() -> String {
    BUILD_INFO
        .get()
        .map_or_else(|| "unknown".to_owned(), |b| b.sha.clone())
}

/// `<channel>-<sha7>`, how toasts name this build.
pub fn build_label() -> String {
    BUILD_INFO.get().map_or_else(
        || "unknown".to_owned(),
        |b| super::update::label(&b.channel, &b.sha),
    )
}

/// Process start, as far as this crate can tell: the first time it is asked.
pub fn process_start() -> Instant {
    *PROCESS_START.get_or_init(Instant::now)
}

/// CPU seconds (user + kernel) consumed by this process so far.
pub fn process_cpu_secs() -> Option<f64> {
    imp::process_cpu_secs()
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn process_cpu_secs() -> Option<f64> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // Fields after the parenthesised comm: state is field 3, utime 14, stime 15.
        let rest = &stat[stat.rfind(')')? + 2..];
        let mut f = rest.split_whitespace();
        let utime: f64 = f.nth(11)?.parse().ok()?;
        let stime: f64 = f.next()?.parse().ok()?;
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        (hz > 0).then(|| (utime + stime) / hz as f64)
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    fn secs(t: FILETIME) -> f64 {
        // 100 ns units.
        ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 1e7
    }

    pub fn process_cpu_secs() -> Option<f64> {
        let (mut created, mut exited) = (FILETIME::default(), FILETIME::default());
        let (mut kernel, mut user) = (FILETIME::default(), FILETIME::default());
        // SAFETY: the pseudo-handle is always valid and the out-pointers are live locals.
        unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        }
        .ok()?;
        Some(secs(kernel) + secs(user))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod imp {
    pub fn process_cpu_secs() -> Option<f64> {
        None
    }
}

/// What `/admin/status` reads from. Cheap to clone.
#[derive(Clone)]
pub struct StatusSource {
    pub agents: SharedAgents,
    pub binds: Arc<Mutex<Vec<SocketAddr>>>,
    pub safe_mode: Arc<AtomicBool>,
    pub presents: Option<Arc<IdleEfficiencyCounters>>,
    /// Log file served by `/admin/logs`.
    pub log_path: std::path::PathBuf,
    /// Compositor capture channel behind `/admin/screenshot`.
    pub capture: Option<crate::operator::screenshot::CaptureEndpoint>,
    /// Relaunch behind `POST /admin/restart`.
    pub restart: Option<crate::operator::handoff::RestartHandle>,
    /// Self-update behind `POST /admin/update`.
    pub update: Option<crate::operator::update::UpdateHandle>,
}

/// Window over which `cpu_pct_2s` is sampled.
pub const CPU_SAMPLE: Duration = Duration::from_secs(2);

fn pct(cpu_delta: f64, wall_delta: f64) -> Value {
    if wall_delta > 0.0 {
        json!((cpu_delta / wall_delta * 100.0 * 10.0).round() / 10.0)
    } else {
        Value::Null
    }
}

impl StatusSource {
    /// Build the status object. Sleeps [`CPU_SAMPLE`] to measure `cpu_pct_2s`
    /// (percent of one core over the window).
    pub async fn render(&self) -> Value {
        let (cpu0, t0) = (process_cpu_secs(), Instant::now());
        tokio::time::sleep(CPU_SAMPLE).await;
        let (cpu1, t1) = (process_cpu_secs(), Instant::now());
        let cpu_pct_2s = match (cpu0, cpu1) {
            (Some(a), Some(b)) => pct(b - a, (t1 - t0).as_secs_f64()),
            _ => Value::Null,
        };
        let uptime = process_start().elapsed();
        let cpu_pct_avg = cpu1.map_or(Value::Null, |c| pct(c, uptime.as_secs_f64()));
        let build = BUILD_INFO.get();
        let agents: Vec<Value> = self
            .agents
            .load()
            .summaries()
            .into_iter()
            .map(|(id, admin)| json!({"id": id, "admin": admin}))
            .collect();
        let binds: Vec<String> = self
            .binds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(ToString::to_string)
            .collect();
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "sha": build.map_or("unknown", |b| b.sha.as_str()),
            "channel": build.map_or("local", |b| b.channel.as_str()),
            "pid": std::process::id(),
            "uptime_s": uptime.as_secs(),
            "binds": binds,
            "agents": agents,
            "safe_mode": self.safe_mode.load(Ordering::Relaxed),
            "safe_mode_hotkey": safe_mode_hotkey().to_json(),
            "frames_presented": self.presents.as_ref().map(|c| c.snapshot().presents),
            "cpu_pct_2s": cpu_pct_2s,
            "cpu_pct_avg": cpu_pct_avg,
            // Outcome of the last update attempt: null, or {ok, sha, error}.
            "last_update": self
                .update
                .as_ref()
                .map_or(Value::Null, |u| u.last_json()),
            // Outcome of the last restart that did not hand over (a successful
            // one ends this process): null, or {ok, pid, error}.
            "last_restart": self
                .restart
                .as_ref()
                .map_or(Value::Null, |r| r.last_json()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed() -> HotkeyStatus {
        HotkeyStatus::Failed {
            chord: "Ctrl+Shift+F12".into(),
            reason: "already in use".into(),
        }
    }

    #[test]
    fn hotkey_status_json_and_banner() {
        let ok = HotkeyStatus::Registered {
            chord: "Ctrl+Shift+F12".into(),
        };
        assert_eq!(
            ok.to_json(),
            json!({"chord": "Ctrl+Shift+F12", "registered": true, "error": null})
        );
        assert_eq!(
            failed().to_json(),
            json!({"chord": "Ctrl+Shift+F12", "registered": false, "error": "already in use"})
        );
        assert_eq!(HotkeyStatus::NotApplicable.to_json(), Value::Null);
        let pending = HotkeyStatus::Pending {
            chord: "Ctrl+Shift+F12".into(),
        };
        assert_eq!(
            pending.to_json(),
            json!({"chord": "Ctrl+Shift+F12", "registered": null, "error": null})
        );
        assert!(pending.banner_line().unwrap().contains("pending"));
        assert!(HotkeyStatus::NotApplicable.banner_line().is_none());
        assert!(ok.banner_line().unwrap().contains("Ctrl+Shift+F12 toggles"));
        let line = failed().banner_line().unwrap();
        assert!(line.contains("NOT registered") && line.contains("already in use"));
    }

    #[test]
    fn hotkey_status_updates_after_pending() {
        let chord = || "Ctrl+Shift+F12".to_string();
        let pending = HotkeyStatus::Pending { chord: chord() };
        let ok = HotkeyStatus::Registered { chord: chord() };
        // Single test owns the global to avoid cross-test races.
        set_safe_mode_hotkey(pending.clone());
        assert_eq!(safe_mode_hotkey(), pending);
        set_safe_mode_hotkey(ok.clone());
        assert_eq!(safe_mode_hotkey(), ok);
        // Late report: the starter's receiver is gone, so the thread records it.
        set_safe_mode_hotkey(pending.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        report_outcome(&tx, failed());
        assert_eq!(safe_mode_hotkey(), failed());
        // Receiver alive: the outcome is delivered, not recorded.
        let (tx, rx) = std::sync::mpsc::channel();
        report_outcome(&tx, ok.clone());
        assert_eq!(rx.recv().unwrap(), ok);
        assert_eq!(safe_mode_hotkey(), failed());
        set_safe_mode_hotkey(ok.clone());
        // A late Pending never downgrades a definitive outcome.
        set_safe_mode_hotkey(pending.clone());
        assert_eq!(safe_mode_hotkey(), ok);
        set_safe_mode_hotkey(failed());
        assert_eq!(safe_mode_hotkey(), failed());
        set_safe_mode_hotkey(HotkeyStatus::NotApplicable);
    }
}
