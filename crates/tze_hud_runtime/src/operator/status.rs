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

static BUILD_INFO: OnceLock<BuildInfo> = OnceLock::new();
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Record the build identity (first call wins) and the process start time.
pub fn set_build_info(info: BuildInfo) {
    process_start();
    let _ = BUILD_INFO.set(info);
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
            "frames_presented": self.presents.as_ref().map(|c| c.snapshot().presents),
            "cpu_pct_2s": cpu_pct_2s,
            "cpu_pct_avg": cpu_pct_avg,
            // No updater yet (separate bead); the key is reserved.
            "last_update": Value::Null,
        })
    }
}
