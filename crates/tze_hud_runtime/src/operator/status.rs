//! `GET /admin/status`: one JSON object describing the running HUD.
//!
//! Carries no secrets: agents appear by id with an `admin` flag, never by PSK
//! or hash.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tze_hud_scene::config::{ChordRole, HudChords, SharedAgents};

use crate::idle_efficiency::IdleEfficiencyCounters;

/// Native desktop presence is admin-only and absent off Windows.
fn tray_status() -> Value {
    #[cfg(target_os = "windows")]
    {
        crate::windowed::shell_presence::tray_status()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Value::Null
    }
}

fn session_locked() -> bool {
    #[cfg(target_os = "windows")]
    {
        crate::windowed::shell_presence::session_locked()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Build identity, registered once by the binary at startup.
#[derive(Debug, Clone)]
pub struct BuildInfo {
    pub sha: String,
    pub channel: String,
}

/// Outcome of registering one global HUD chord ([`ChordRole`]).
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
    /// The chord could not be registered (typically another program owns it);
    /// The tray remains an independent human pause path on Windows.
    Failed {
        chord: String,
        reason: String,
    },
}

impl HotkeyStatus {
    /// One banner line for `role`'s chord, or `None` when there is nothing to say.
    pub fn banner_line(&self, role: ChordRole) -> Option<String> {
        let (label, does, lost) = match role {
            ChordRole::Pause => ("safe", "pauses agents", "use the tray icon's Pause"),
            ChordRole::Panel => ("panel", "opens the control panel", "use the pause chord"),
            ChordRole::Pairing => ("pair", "opens Connect an agent", "use the control panel"),
            ChordRole::Answer => (
                "answer",
                "focuses the newest question",
                "use the card buttons",
            ),
            ChordRole::Hide => (
                "hide",
                "toggles Hide all",
                "use Hide all in the panel or tray",
            ),
        };
        match self {
            Self::NotApplicable => None,
            Self::Registered { chord } => Some(format!("   {label:<7}: {chord} {does}")),
            Self::Pending { chord } => Some(format!(
                "   {label:<7}: hotkey {chord} registration pending (see /admin/status)"
            )),
            Self::Failed { chord, reason } => Some(format!(
                "   {label:<7}: WARNING hotkey {chord} NOT registered ({reason}); {lost}"
            )),
        }
    }

    pub(crate) fn failure_detail(&self, role: ChordRole) -> Option<String> {
        match self {
            Self::Failed { reason, .. } if role == ChordRole::Pause => {
                Some(format!("{reason}; use the tray icon's Pause"))
            }
            Self::Failed { reason, .. } => Some(reason.clone()),
            _ => None,
        }
    }

    fn to_json_for(&self, role: ChordRole) -> Value {
        let mut value = self.to_json();
        if let Some(detail) = self.failure_detail(role) {
            value["error"] = Value::String(detail);
        }
        value
    }

    /// A `*_hotkey` value for `/admin/status`: `null` when not applicable.
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

static HOTKEYS: Mutex<[HotkeyStatus; ChordRole::ALL.len()]> = Mutex::new([
    HotkeyStatus::NotApplicable,
    HotkeyStatus::NotApplicable,
    HotkeyStatus::NotApplicable,
    HotkeyStatus::NotApplicable,
    HotkeyStatus::NotApplicable,
]);

fn slot(role: ChordRole) -> usize {
    match role {
        ChordRole::Pause => 0,
        ChordRole::Panel => 1,
        ChordRole::Pairing => 2,
        ChordRole::Answer => 3,
        ChordRole::Hide => 4,
    }
}

/// Record `role`'s registration outcome and log it. Updateable: the hotkey
/// thread may report after the starter gave up waiting. A pause-chord failure
/// is logged at error level so the owner sees the tray fallback. A
/// `Pending` never replaces a definitive outcome (the thread can win the race
/// against the starter).
pub fn set_hotkey(role: ChordRole, status: HotkeyStatus) -> HotkeyStatus {
    let mut all = HOTKEYS.lock().unwrap_or_else(|e| e.into_inner());
    let cur = &mut all[slot(role)];
    if matches!(status, HotkeyStatus::Pending { .. })
        && matches!(
            *cur,
            HotkeyStatus::Registered { .. } | HotkeyStatus::Failed { .. }
        )
    {
        return cur.clone();
    }
    let key = role.config_key();
    match &status {
        HotkeyStatus::Failed { chord, reason } if role == ChordRole::Pause => {
            tracing::error!(%chord, %reason, key, "safe-mode hotkey NOT registered: use the tray icon's Pause");
        }
        HotkeyStatus::Failed { chord, reason } => {
            tracing::warn!(%chord, %reason, key, "global hotkey NOT registered");
        }
        HotkeyStatus::Pending { chord } => {
            tracing::warn!(%chord, key, "global hotkey registration pending");
        }
        HotkeyStatus::Registered { chord } => {
            tracing::info!(%chord, key, "global hotkey registered");
        }
        HotkeyStatus::NotApplicable => {}
    }
    *cur = status.clone();
    status
}

/// `role`'s outcome when the config left it unbound (its default is another
/// role's explicit chord): not registered, saying which key to set.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn unbound_status(chords: &HudChords, role: ChordRole) -> Option<HotkeyStatus> {
    if chords.is_bound(role) {
        return None;
    }
    let chord = chords.get(role);
    let taken = chords
        .role_of(&chord)
        .map_or("another chord", |owner| owner.config_key());
    Some(HotkeyStatus::Failed {
        chord: chord.to_string(),
        reason: format!(
            "not bound: {chord} is runtime.{taken}; set runtime.{} to bind one",
            role.config_key()
        ),
    })
}

/// Hand `role`'s outcome to the starter waiting on `tx`. If the starter has
/// already stopped waiting (timed out with `Pending`), record it directly so
/// the late result supersedes `Pending`.
pub fn report_outcome(
    tx: &std::sync::mpsc::Sender<(ChordRole, HotkeyStatus)>,
    role: ChordRole,
    status: HotkeyStatus,
) {
    if let Err(std::sync::mpsc::SendError((role, status))) = tx.send((role, status)) {
        set_hotkey(role, status);
    }
}

/// Native overlay observations, sampled on demand rather than per frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct OverlayWindowState {
    pub visible: bool,
    pub iconic: bool,
    pub topmost: bool,
    /// `None` when DWM cannot report its state.
    pub cloaked: Option<bool>,
}

impl OverlayWindowState {
    #[cfg(any(target_os = "windows", test))]
    pub(crate) fn needs_reassert(self) -> bool {
        !self.visible || self.iconic || !self.topmost || self.cloaked == Some(true)
    }
}

type WindowStateProbe =
    Arc<dyn Fn() -> std::collections::HashMap<String, OverlayWindowState> + Send + Sync>;
static WINDOW_STATE_PROBE: Mutex<Option<WindowStateProbe>> = Mutex::new(None);

/// The window thread installs a probe holding weak window references. The HTTP
/// thread only reads native state; any repair is queued back to the window thread.
#[cfg(target_os = "windows")]
pub(crate) fn set_window_state_probe(probe: WindowStateProbe) {
    *WINDOW_STATE_PROBE.lock().unwrap_or_else(|e| e.into_inner()) = Some(probe);
}

/// One overlaid display for `/admin/status`; its list index is the
/// `/admin/screenshot?display=<i>` index (0 = primary).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DisplayStatus {
    pub name: String,
    /// Origin in scene pixels (the primary's top-left is 0,0).
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    /// Zones configured onto this display (`[displays.<NAME>]`).
    pub zones: Vec<String>,
    /// Null outside Windows, or when the native window is unavailable.
    pub window_state: Option<OverlayWindowState>,
}

/// A configured zone placement (`[displays.<NAME>]`) that has no overlay
/// window; the zone renders on the primary instead.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct UnplacedZone {
    pub zone: String,
    /// The display name the zone is configured for.
    pub display: String,
    pub reason: UnplacedReason,
}

/// Why a configured display has no overlay window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnplacedReason {
    /// No connected monitor has that name.
    NotConnected,
    /// The monitor is connected, but its overlay surface kept failing and the
    /// runtime stopped recreating it.
    OverlayFailed,
}

static DISPLAYS: Mutex<(Vec<DisplayStatus>, Vec<UnplacedZone>)> =
    Mutex::new((Vec::new(), Vec::new()));

/// Record the overlaid displays (main thread, on startup and every change).
pub fn set_displays(displays: Vec<DisplayStatus>, unplaced_zones: Vec<UnplacedZone>) {
    *DISPLAYS.lock().unwrap_or_else(|e| e.into_inner()) = (displays, unplaced_zones);
}

/// Overlaid display rects in scene pixels, in `/admin/status` order.
pub(crate) fn display_rects() -> Vec<tze_hud_scene::Rect> {
    DISPLAYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .0
        .iter()
        .map(|d| tze_hud_scene::Rect::new(d.x as f32, d.y as f32, d.width as f32, d.height as f32))
        .collect()
}

/// The name of display `index` in `/admin/status` order, if connected.
pub fn display_name(index: usize) -> Option<String> {
    DISPLAYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .0
        .get(index)
        .map(|d| d.name.clone())
}

fn displays_json() -> (Value, Value) {
    let (mut displays, unplaced) = DISPLAYS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let probe = WINDOW_STATE_PROBE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(probe) = probe {
        let states = probe();
        for display in &mut displays {
            display.window_state = states.get(&display.name).copied();
        }
    }
    (json!(displays), json!(unplaced))
}

/// `role`'s recorded outcome, `NotApplicable` until one is set.
pub fn hotkey(role: ChordRole) -> HotkeyStatus {
    HOTKEYS.lock().unwrap_or_else(|e| e.into_inner())[slot(role)].clone()
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

/// The release channel this build came from (`dev`, `stable`, …),
/// `"dev"` before [`set_build_info`].
pub fn build_channel() -> String {
    BUILD_INFO
        .get()
        .map_or_else(|| "dev".to_owned(), |b| b.channel.clone())
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
    /// One admin-only UI snapshot (null blocks without a windowed runtime).
    pub surfaces: Option<crate::operator::surface_status::SurfaceProbe>,
    /// Log file served by `/admin/logs`.
    pub log_path: std::path::PathBuf,
    /// Compositor capture channel behind `/admin/screenshot`.
    pub capture: Option<crate::operator::screenshot::CaptureEndpoint>,
    /// Relaunch behind `POST /admin/restart`.
    pub restart: Option<crate::operator::handoff::RestartHandle>,
    /// Self-update behind `POST /admin/update`.
    pub update: Option<crate::operator::update::UpdateHandle>,
    /// First-run pairing behind `POST /pair`.
    pub pairing: Option<Arc<crate::pairing::Pairing>>,
    /// Windows Firewall check behind `tailnet_inbound`.
    pub firewall: Arc<crate::firewall::FirewallProbe>,
    /// Live design tokens behind `config_reload`.
    pub(crate) appearance: Option<Arc<crate::appearance::Appearance>>,
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
    /// Build the status object. With `sample_cpu`, sleeps [`CPU_SAMPLE`] to
    /// measure `cpu_pct_2s` (percent of one core over the window); without
    /// it, answers at once with `cpu_pct_2s` null (polling harnesses).
    pub async fn render(&self, sample_cpu: bool) -> Value {
        let (cpu0, t0) = (process_cpu_secs(), Instant::now());
        if sample_cpu {
            tokio::time::sleep(CPU_SAMPLE).await;
        }
        let (cpu1, t1) = (process_cpu_secs(), Instant::now());
        let cpu_pct_2s = match (cpu0, cpu1) {
            (Some(a), Some(b)) if sample_cpu => pct(b - a, (t1 - t0).as_secs_f64()),
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
        // COM enumeration is slow on a cache miss; keep it off the async workers.
        let firewall = Arc::clone(&self.firewall);
        let tailnet_inbound = tokio::task::spawn_blocking(move || firewall.current())
            .await
            .unwrap_or_else(|e| crate::firewall::TailnetInbound::Unknown {
                error: e.to_string(),
            });
        let (displays, unplaced_zones) = displays_json();
        // Read off the async workers: the probe may wait briefly for the scene.
        let surfaces = match self.surfaces.clone() {
            Some(probe) => tokio::task::spawn_blocking(move || probe.read())
                .await
                .unwrap_or_default(),
            None => Default::default(),
        };
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "sha": build.map_or("unknown", |b| b.sha.as_str()),
            "channel": build.map_or("local", |b| b.channel.as_str()),
            "pid": std::process::id(),
            "uptime_s": uptime.as_secs(),
            "binds": binds,
            // Whether Windows Firewall lets remote agents reach the tailnet bind.
            "tailnet_inbound": tailnet_inbound.to_json(),
            "agents": agents,
            "safe_mode": self.safe_mode.load(Ordering::Relaxed),
            "tray": tray_status(),
            "session_locked": session_locked(),
            "safe_mode_hotkey": hotkey(ChordRole::Pause).to_json_for(ChordRole::Pause),
            "panel_hotkey": hotkey(ChordRole::Panel).to_json_for(ChordRole::Panel),
            "pairing_hotkey": hotkey(ChordRole::Pairing).to_json_for(ChordRole::Pairing),
            "answer_hotkey": hotkey(ChordRole::Answer).to_json_for(ChordRole::Answer),
            "hide_hotkey": hotkey(ChordRole::Hide).to_json_for(ChordRole::Hide),
            // Hide from screen capture: null off Windows.
            "capture_exclusion": crate::capture_exclusion::status(),
            "displays": displays,
            "unplaced_zones": unplaced_zones,
            "frames_presented": self.presents.as_ref().map(|c| c.snapshot().presents),
            // Milliseconds since the last presented frame: a settle signal.
            "idle_since_ms": self.presents.as_ref().map(|c| c.idle_since_ms()),
            // Portal UI state for out-of-process validation (admin only).
            "portals": surfaces.portals,
            "notifications": surfaces.notifications,
            "widgets": surfaces.widgets,
            "link_launches": surfaces.link_launches,
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
            // The last `[design_tokens]` reload from the config file that
            // changed something or failed: null, or {ok, error?, at_ms}.
            "config_reload": self
                .appearance
                .as_ref()
                .and_then(|a| a.reload_status())
                .map_or(Value::Null, |s| json!(s)),
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
        assert!(
            pending
                .banner_line(ChordRole::Pause)
                .unwrap()
                .contains("pending")
        );
        assert!(
            HotkeyStatus::NotApplicable
                .banner_line(ChordRole::Pause)
                .is_none()
        );
        assert!(
            ok.banner_line(ChordRole::Pause)
                .unwrap()
                .contains("Ctrl+Shift+F12 pauses agents")
        );
        let answer = HotkeyStatus::Registered {
            chord: "Ctrl+Shift+F9".into(),
        };
        assert!(
            answer
                .banner_line(ChordRole::Answer)
                .unwrap()
                .contains("Ctrl+Shift+F9 focuses")
        );
        let line = failed().banner_line(ChordRole::Pause).unwrap();
        assert!(line.contains("NOT registered") && line.contains("already in use"));
        assert!(line.contains("use the tray icon's Pause"));
        assert_eq!(
            failed().to_json_for(ChordRole::Pause)["error"],
            "already in use; use the tray icon's Pause"
        );
        // R5.1 S6: a refused hide chord names the panel and tray paths.
        let hide = HotkeyStatus::Failed {
            chord: "Ctrl+Shift+F8".into(),
            reason: "already in use".into(),
        };
        let line = hide.banner_line(ChordRole::Hide).unwrap();
        assert!(
            line.contains("hide") && line.contains("NOT registered"),
            "{line}"
        );
        assert!(line.contains("use Hide all in the panel or tray"), "{line}");
        assert_eq!(
            hide.to_json_for(ChordRole::Hide),
            json!({"chord": "Ctrl+Shift+F8", "registered": false, "error": "already in use"})
        );
        #[cfg(not(target_os = "windows"))]
        {
            assert_eq!(tray_status(), Value::Null);
            assert!(!session_locked());
            assert_eq!(crate::capture_exclusion::status(), Value::Null);
        }
    }

    /// A role whose default another role took explicitly reports
    /// registered:false with the keys to change; bound roles report nothing.
    #[test]
    fn an_unbound_default_reports_why() {
        let mut chords = HudChords::default();
        chords.set(ChordRole::Answer, chords.hide);
        chords.unbind(ChordRole::Hide);
        assert_eq!(unbound_status(&chords, ChordRole::Answer), None);
        let status = unbound_status(&chords, ChordRole::Hide).unwrap();
        assert_eq!(
            status.to_json_for(ChordRole::Hide),
            json!({
                "chord": "Ctrl+Shift+F8",
                "registered": false,
                "error": "not bound: Ctrl+Shift+F8 is runtime.answer_hotkey; set runtime.hide_hotkey to bind one"
            })
        );
        assert!(
            status
                .banner_line(ChordRole::Hide)
                .unwrap()
                .contains("NOT registered (not bound")
        );
    }

    #[tokio::test]
    async fn hotkey_status_updates_after_pending() {
        let chord = || "Ctrl+Shift+F12".to_string();
        let pending = HotkeyStatus::Pending { chord: chord() };
        let ok = HotkeyStatus::Registered { chord: chord() };
        // Single test owns the global to avoid cross-test races.
        let pause = ChordRole::Pause;
        set_hotkey(pause, pending.clone());
        assert_eq!(hotkey(pause), pending);
        set_hotkey(pause, ok.clone());
        assert_eq!(hotkey(pause), ok);
        // Roles are independent slots.
        let answer = HotkeyStatus::Registered {
            chord: "Ctrl+Shift+F9".into(),
        };
        set_hotkey(ChordRole::Answer, answer.clone());
        assert_eq!(hotkey(ChordRole::Answer), answer);
        assert_eq!(hotkey(pause), ok);
        set_hotkey(ChordRole::Answer, HotkeyStatus::NotApplicable);
        assert_eq!(hotkey(ChordRole::Panel), HotkeyStatus::NotApplicable);
        // Late report: the starter's receiver is gone, so the thread records it.
        set_hotkey(pause, pending.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        report_outcome(&tx, pause, failed());
        assert_eq!(hotkey(pause), failed());
        // Receiver alive: the outcome is delivered, not recorded.
        let (tx, rx) = std::sync::mpsc::channel();
        report_outcome(&tx, pause, ok.clone());
        assert_eq!(rx.recv().unwrap(), (pause, ok.clone()));
        assert_eq!(hotkey(pause), failed());
        set_hotkey(pause, ok.clone());
        // A late Pending never downgrades a definitive outcome.
        set_hotkey(pause, pending.clone());
        assert_eq!(hotkey(pause), ok);
        set_hotkey(pause, failed());
        assert_eq!(hotkey(pause), failed());
        use crate::shell::control_panel::row::Section;
        let input = crate::shell::control_panel::InputSection::new(
            crate::shell::control_panel::chords::ChordControl::new(Default::default(), None),
            false,
            None,
        );
        let row = input
            .rows()
            .await
            .into_iter()
            .find(|row| row.id == "input.pause_chord")
            .unwrap();
        assert!(row.detail.unwrap().contains("use the tray icon's Pause"));
        set_hotkey(pause, HotkeyStatus::NotApplicable);
    }
}
