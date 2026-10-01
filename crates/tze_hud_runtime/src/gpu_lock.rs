//! # gpu_lock
//!
//! Windows GPU scheduling lock for tze_hud.exe.
//!
//! ## Background
//!
//! The tzehouse-windows box (`windows-host.example`, RTX 3080) is shared
//! between interactive `/user-test` sessions and scripted GPU runs (perf and
//! readback proofs). Running them concurrently can cause DXGI device-lost
//! errors or skewed measurements. A file-based advisory lock provides mutual
//! exclusion.
//!
//! ## Lock file
//!
//! Path: `C:\ProgramData\tze_hud\gpu.lock`
//!
//! Format (line-delimited UTF-8 key=value):
//! ```text
//! SESSION_TYPE=interactive
//! PID=<process-id>
//! STARTED_AT=<RFC3339 UTC timestamp>
//! DESCRIPTION=tze_hud.exe interactive session
//! ```
//!
//! ## Usage
//!
//! ```rust,ignore
//! use tze_hud_runtime::gpu_lock::GpuLock;
//!
//! // Acquire near the top of main().
//! // Returns Ok(None) on non-Windows platforms (no-op).
//! // Returns Ok(Some(guard)) on Windows — hold the guard for the process lifetime.
//! // Returns Err if the lock is held by another live process.
//! let _gpu_lock_guard = GpuLock::acquire()?;
//! ```
//!
//! The returned [`GpuLockGuard`] releases the lock on Drop. A panic hook is
//! installed automatically so the lock is also released on unwind.
//!
//! ## Cross-platform
//!
//! On non-Windows targets, [`GpuLock::acquire`] is a no-op that returns
//! `Ok(None)`. All types compile on every platform; call sites need no cfg guards.
//!
//! ## Fail-safe
//!
//! If lock file I/O fails (permissions, missing directory, etc.), the function
//! logs a warning and continues startup — except when the lock is held by
//! another live process, which is a hard refusal.
//!
//! Design doc: `docs/design/tzehouse-windows-gpu-scheduling.md`

use std::fmt;

// ─── Error type ───────────────────────────────────────────────────────────────

/// Errors that cause `GpuLock::acquire` to refuse startup.
///
/// Transient I/O errors are logged as warnings but do not produce this error —
/// only a live conflicting lock does.
#[derive(Debug)]
pub struct GpuLockConflict {
    /// The SESSION_TYPE value in the existing lock file.
    pub session_type: String,
    /// The PID in the existing lock file.
    pub pid: u32,
    /// The STARTED_AT timestamp in the existing lock file.
    pub started_at: String,
    /// The DESCRIPTION value in the existing lock file.
    pub description: String,
}

impl fmt::Display for GpuLockConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[gpu-lock] GPU is already in use: SESSION_TYPE={} PID={} STARTED_AT={} — {}",
            self.session_type, self.pid, self.started_at, self.description
        )
    }
}

impl std::error::Error for GpuLockConflict {}

// ─── Public API (cross-platform) ─────────────────────────────────────────────

/// RAII guard that holds the GPU lock for the duration of `tze_hud.exe`.
///
/// Dropping this guard releases the lock atomically, verifying PID ownership
/// before deletion so a lock taken over by another process is never blown away.
///
/// On non-Windows platforms, the guard is a zero-size no-op struct.
#[derive(Debug)]
pub struct GpuLockGuard {
    #[cfg(target_os = "windows")]
    inner: windows_impl::GpuLockGuardInner,
    #[cfg(not(target_os = "windows"))]
    _private: (),
}

impl Drop for GpuLockGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        self.inner.release();
    }
}

/// The GPU lock manager.
///
/// Call [`GpuLock::acquire`] once at startup and hold the returned guard until
/// the process exits.
pub struct GpuLock;

impl GpuLock {
    /// Acquire the GPU lock for an interactive `tze_hud.exe` session.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(guard))` — lock acquired; hold the guard for the process lifetime.
    /// - `Ok(None)` — not Windows; no-op.
    /// - `Err(GpuLockConflict)` — lock held by another live process; refuse startup.
    ///
    /// Transient I/O errors (missing directory, permissions, etc.) are logged
    /// as warnings but do not produce `Err` — the runtime continues without a lock
    /// rather than refusing to start over a filesystem issue. The single exception
    /// is a live conflicting lock, which is a hard refusal.
    pub fn acquire() -> Result<Option<GpuLockGuard>, GpuLockConflict> {
        #[cfg(target_os = "windows")]
        {
            windows_impl::acquire().map(|inner| Some(GpuLockGuard { inner }))
        }
        #[cfg(not(target_os = "windows"))]
        {
            Ok(None)
        }
    }
}

// ─── Cross-platform lock-staleness logic (unit-testable) ─────────────────────
//
// "Is an existing lock stale, or held by a genuinely-live tze_hud?" is a pure
// decision given three inputs: the lock's STARTED_AT, the system boot instant,
// and facts about the process currently occupying the lock's PID. Keeping that
// decision here — free of Win32 calls — lets it be unit-tested on any platform.
// The Windows layer only gathers the inputs and acts on the verdict.
//
// Compiled on Windows (where it is used) and in test builds (where it is
// exercised); on other non-test builds it would be dead code, so it is gated.
#[cfg(any(target_os = "windows", test))]
mod lock_logic {
    /// Small tolerance (seconds) applied to the boot comparison so second-level
    /// truncation and the ~1s jitter in deriving the boot instant never let us
    /// reclaim a lock a genuinely-live process wrote moments *after* boot.
    const BOOT_SKEW_SECS: i64 = 5;

    /// Tolerance (seconds) between a process's creation time and the lock's
    /// STARTED_AT. The lock is written shortly after the process starts, so a
    /// match within this window confirms the live PID is the lock's author; a
    /// gross mismatch means the PID was recycled by an unrelated launch.
    const CREATION_MATCH_TOLERANCE_SECS: i64 = 120;

    /// Facts about the process currently occupying a lock's PID.
    pub(super) struct ProcessFacts {
        /// True when the process image name is `tze_hud.exe`.
        pub image_is_tze_hud: bool,
        /// Process creation time as Unix seconds (UTC), if it could be read.
        pub creation_unix: Option<i64>,
    }

    /// Classification of an existing lock file.
    pub(super) enum LockClass {
        /// The lock can be reclaimed (writer is provably gone or PID recycled).
        Stale,
        /// A genuinely-live tze_hud holds the lock; startup must be refused.
        LiveConflict,
    }

    /// Unix seconds for a proleptic-Gregorian UTC date-time (Hinnant's civil
    /// algorithm). Valid for every date this module encounters.
    pub(super) fn civil_to_unix(y: i64, m: i64, d: i64, hh: i64, mm: i64, ss: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = (if y >= 0 { y } else { y - 399 }) / 400;
        let yoe = y - era * 400; // [0, 399]
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
        let days = era * 146097 + doe - 719468;
        days * 86400 + hh * 3600 + mm * 60 + ss
    }

    /// Parse a `YYYY-MM-DDTHH:MM:SSZ` timestamp (the format `write_lock` emits)
    /// into Unix seconds. Returns `None` on anything that doesn't match, so an
    /// unparseable STARTED_AT never drives a reclaim decision on its own.
    pub(super) fn parse_rfc3339_utc(s: &str) -> Option<i64> {
        let s = s.trim();
        let (date, rest) = s.split_once('T')?;
        let mut dparts = date.split('-');
        let y: i64 = dparts.next()?.parse().ok()?;
        let mo: i64 = dparts.next()?.parse().ok()?;
        let d: i64 = dparts.next()?.parse().ok()?;
        // Time may carry a trailing 'Z' and/or fractional seconds; keep HH:MM:SS.
        let time = rest.trim_end_matches('Z');
        let time = time.split('.').next()?;
        let mut tparts = time.split(':');
        let hh: i64 = tparts.next()?.parse().ok()?;
        let mm: i64 = tparts.next()?.parse().ok()?;
        let ss: i64 = tparts.next()?.parse().ok()?;
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
            return None;
        }
        Some(civil_to_unix(y, mo, d, hh, mm, ss))
    }

    /// True when the lock's STARTED_AT is confidently before the system boot
    /// instant — the writing process cannot have survived the reboot, so the
    /// lock is stale by definition. Unparseable timestamps return false.
    pub(super) fn started_at_predates_boot(started_at: &str, boot_unix: i64) -> bool {
        match parse_rfc3339_utc(started_at) {
            Some(t) => t + BOOT_SKEW_SECS < boot_unix,
            None => false,
        }
    }

    /// Whether the live process occupying the lock's PID is the tze_hud that
    /// authored this lock: it must be `tze_hud.exe`, and (when both times are
    /// known) its creation time must line up with STARTED_AT. A recycled PID
    /// owned by an unrelated image, or a wildly different creation time, fails.
    pub(super) fn pid_is_lock_owner(facts: &ProcessFacts, started_at: &str) -> bool {
        if !facts.image_is_tze_hud {
            return false;
        }
        match (facts.creation_unix, parse_rfc3339_utc(started_at)) {
            (Some(created), Some(started)) => {
                (started - created).abs() <= CREATION_MATCH_TOLERANCE_SECS
            }
            // Missing either time: fall back to the image-name match alone.
            _ => true,
        }
    }

    /// Classify an existing lock. `boot_unix` is `None` when the boot instant
    /// could not be determined; `live_facts` is `None` when no live process
    /// occupies the lock's PID.
    ///
    /// Boot-time is the primary defense: a lock predating boot is stale
    /// regardless of what now occupies its PID. Only a live `tze_hud.exe` whose
    /// creation time matches STARTED_AT is treated as a genuine holder.
    pub(super) fn classify_existing_lock(
        started_at: &str,
        boot_unix: Option<i64>,
        live_facts: Option<&ProcessFacts>,
    ) -> LockClass {
        if let Some(boot) = boot_unix {
            if started_at_predates_boot(started_at, boot) {
                return LockClass::Stale;
            }
        }
        match live_facts {
            Some(facts) if pid_is_lock_owner(facts, started_at) => LockClass::LiveConflict,
            _ => LockClass::Stale,
        }
    }
}

// ─── Windows implementation ───────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::GpuLockConflict;
    use super::lock_logic::{self, LockClass, ProcessFacts};
    use std::path::{Path, PathBuf};

    const LOCK_DIR: &str = r"C:\ProgramData\tze_hud";
    const LOCK_FILE: &str = r"C:\ProgramData\tze_hud\gpu.lock";
    const LOCK_TMP: &str = r"C:\ProgramData\tze_hud\gpu.lock.tmp";

    /// Inner guard state: the lock file path and the PID that wrote it.
    ///
    /// The struct holds the PID (not re-read from disk on drop) so release
    /// is correct even if the file is corrupt at that point.
    #[derive(Debug)]
    pub(super) struct GpuLockGuardInner {
        lock_path: PathBuf,
        owner_pid: u32,
    }

    impl GpuLockGuardInner {
        /// Release the lock. Verifies PID ownership before deletion.
        pub(super) fn release(&self) {
            release_lock(&self.lock_path, self.owner_pid);
        }
    }

    // ── Lock content helpers ──────────────────────────────────────────────────

    /// Parse the key=value pairs in the lock file. Returns None on any error.
    fn parse_lock_file(path: &Path) -> Option<std::collections::HashMap<String, String>> {
        let content = std::fs::read_to_string(path).ok()?;
        let mut map = std::collections::HashMap::new();
        for line in content.lines() {
            if let Some((key, value)) = line.split_once('=') {
                map.insert(key.trim().to_string(), value.to_string());
            }
        }
        if map.is_empty() { None } else { Some(map) }
    }

    /// Return an RFC 3339 UTC timestamp for "right now".
    ///
    /// Uses `GetSystemTime` from the `windows` crate (already a dependency of
    /// `tze_hud_runtime` on Windows). Falls back to a fixed placeholder string
    /// if the call fails — the placeholder is still parseable key=value.
    fn utc_now_rfc3339() -> String {
        use windows::Win32::System::SystemInformation::GetSystemTime;

        // SAFETY: GetSystemTime returns the current UTC SYSTEMTIME and has no
        // preconditions for callers.
        let st = unsafe { GetSystemTime() };

        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond,
        )
    }

    /// Write the lock file content (SESSION_TYPE, PID, STARTED_AT, DESCRIPTION).
    ///
    /// Uses a write-to-tmp-then-rename pattern for best-effort atomicity on NTFS.
    fn write_lock(session_type: &str, pid: u32, started_at: &str, description: &str) -> bool {
        let content = format!(
            "SESSION_TYPE={session_type}\nPID={pid}\nSTARTED_AT={started_at}\nDESCRIPTION={description}\n"
        );
        // Write to tmp first, then rename.
        if let Err(e) = std::fs::write(LOCK_TMP, &content) {
            tracing::warn!(
                error = %e,
                "[gpu-lock] Failed to write lock tmp file {LOCK_TMP}; continuing without lock"
            );
            return false;
        }
        if let Err(e) = std::fs::rename(LOCK_TMP, LOCK_FILE) {
            tracing::warn!(
                error = %e,
                "[gpu-lock] Failed to rename {LOCK_TMP} → {LOCK_FILE}; continuing without lock"
            );
            let _ = std::fs::remove_file(LOCK_TMP);
            return false;
        }
        true
    }

    /// System boot instant as Unix seconds (UTC), or `None` if it cannot be
    /// derived. Computed as `now - uptime`: `GetTickCount64` gives milliseconds
    /// since boot, so subtracting it from the current UTC time yields the boot
    /// instant — the same quantity as `Win32_OperatingSystem.LastBootUpTime`
    /// that the shipped `hud_vm_env.sh` workaround compared against.
    fn system_boot_unix() -> Option<i64> {
        use windows::Win32::System::SystemInformation::GetTickCount64;
        // SAFETY: GetTickCount64 has no preconditions; returns ms since boot.
        let uptime_ms = unsafe { GetTickCount64() };
        let now = system_time_now_unix()?;
        Some(now - (uptime_ms / 1000) as i64)
    }

    /// Current UTC time as Unix seconds via `GetSystemTime`.
    fn system_time_now_unix() -> Option<i64> {
        use windows::Win32::System::SystemInformation::GetSystemTime;
        // SAFETY: GetSystemTime returns the current UTC SYSTEMTIME; no preconditions.
        let st = unsafe { GetSystemTime() };
        Some(lock_logic::civil_to_unix(
            st.wYear as i64,
            st.wMonth as i64,
            st.wDay as i64,
            st.wHour as i64,
            st.wMinute as i64,
            st.wSecond as i64,
        ))
    }

    /// Observe the process currently occupying `pid`: image name and
    /// (best-effort) creation time. Returns `None` when no such process
    /// exists — i.e. the PID is dead and any lock naming it is stale.
    ///
    /// This replaces a bare `OpenProcess` liveness probe: after a reboot,
    /// Windows aggressively recycles PIDs, so an unrelated process inheriting
    /// the lock's PID would pass a bare liveness check. Checking the image name
    /// (and creation time) is what distinguishes a real tze_hud holder from a
    /// recycled PID.
    fn observe_process(pid: u32) -> Option<ProcessFacts> {
        let image = process_image_name(pid)?;
        Some(ProcessFacts {
            image_is_tze_hud: image.eq_ignore_ascii_case("tze_hud.exe"),
            creation_unix: process_creation_unix(pid),
        })
    }

    /// Return the image (exe) file name for a live PID, or `None` if the PID is
    /// not present in the process table. Uses the same Toolhelp snapshot pattern
    /// as [`find_existing_tze_hud_process`].
    fn process_image_name(pid: u32) -> Option<String> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        };

        // SAFETY: CreateToolhelp32Snapshot has no memory safety preconditions.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;

        // SAFETY: PROCESSENTRY32W is plain-old-data; dwSize must be set first.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut name = None;
        // SAFETY: snapshot is valid and entry points to initialized storage.
        if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
            loop {
                if entry.th32ProcessID == pid {
                    let nul = entry
                        .szExeFile
                        .iter()
                        .position(|ch| *ch == 0)
                        .unwrap_or(entry.szExeFile.len());
                    name = Some(String::from_utf16_lossy(&entry.szExeFile[..nul]));
                    break;
                }
                // SAFETY: snapshot valid and entry remains initialized storage.
                if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                    break;
                }
            }
        }

        // SAFETY: snapshot is a valid handle from CreateToolhelp32Snapshot.
        unsafe { CloseHandle(snapshot).ok() };
        name
    }

    /// Best-effort process creation time as Unix seconds (UTC) for `pid`.
    /// Returns `None` if the process cannot be opened or queried.
    fn process_creation_unix(pid: u32) -> Option<i64> {
        use windows::Win32::Foundation::{CloseHandle, FILETIME};
        use windows::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        // SAFETY: OpenProcess with a well-known access mask is safe.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let mut creation = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exit = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut kernel = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut user = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        // SAFETY: handle is valid; all four FILETIME out-params are initialized.
        let res =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
        // SAFETY: handle is a valid process handle returned by OpenProcess.
        unsafe { CloseHandle(handle).ok() };
        res.ok()?;
        Some(filetime_to_unix(&creation))
    }

    /// Convert a Win32 FILETIME (100-ns ticks since 1601-01-01 UTC) to Unix seconds.
    fn filetime_to_unix(ft: &windows::Win32::Foundation::FILETIME) -> i64 {
        const TICKS_PER_SEC: u64 = 10_000_000;
        const EPOCH_DIFF_SECS: i64 = 11_644_473_600; // 1601-01-01 → 1970-01-01
        let ticks = ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64);
        (ticks / TICKS_PER_SEC) as i64 - EPOCH_DIFF_SECS
    }

    /// Find another live `tze_hud.exe` process that may predate the lock file.
    ///
    /// The GPU lock was added after some deployed HUD binaries already existed.
    /// If one of those older processes is still running, a fresh process could
    /// otherwise acquire a new lock file and race the existing overlay for the
    /// same GPU/window resources. Treating the already-running binary as a lock
    /// holder makes this failure mode explicit at startup.
    fn find_existing_tze_hud_process(exclude_pid: u32) -> Option<u32> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        };

        // SAFETY: CreateToolhelp32Snapshot has no memory safety preconditions.
        let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
            Ok(handle) => handle,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "[gpu-lock] Could not enumerate processes for pre-lock HUD detection"
                );
                return None;
            }
        };

        // SAFETY: PROCESSENTRY32W is a plain old data Win32 struct. The API
        // requires dwSize to be initialized before the first call.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut found = None;
        // SAFETY: snapshot is a valid handle and entry points to initialized storage.
        if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
            loop {
                if entry.th32ProcessID != exclude_pid && exe_name_is_tze_hud(&entry.szExeFile) {
                    found = Some(entry.th32ProcessID);
                    break;
                }
                // SAFETY: snapshot is valid and entry remains initialized storage.
                if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                    break;
                }
            }
        }

        // SAFETY: snapshot is a valid handle returned by CreateToolhelp32Snapshot.
        unsafe { CloseHandle(snapshot).ok() };
        found
    }

    fn exe_name_is_tze_hud(raw_name: &[u16]) -> bool {
        let nul = raw_name
            .iter()
            .position(|ch| *ch == 0)
            .unwrap_or(raw_name.len());
        let name = String::from_utf16_lossy(&raw_name[..nul]);
        name.eq_ignore_ascii_case("tze_hud.exe")
    }

    // ── Lock release helper (shared by Drop and panic hook) ───────────────────

    fn release_lock(lock_path: &Path, owner_pid: u32) {
        tracing::info!("[gpu-lock] Releasing {}", lock_path.display());

        // Re-read the lock file to verify PID ownership before deletion.
        match parse_lock_file(lock_path) {
            None => {
                // Lock file already absent or unreadable — nothing to do.
                tracing::info!("[gpu-lock] Lock file absent or unreadable; nothing to release.");
            }
            Some(fields) => {
                let file_pid: u32 = fields.get("PID").and_then(|v| v.parse().ok()).unwrap_or(0);
                if file_pid != owner_pid {
                    tracing::warn!(
                        our_pid = owner_pid,
                        lock_pid = file_pid,
                        "[gpu-lock] Lock PID mismatch — another process now holds the lock. Not deleting."
                    );
                    return;
                }
                if let Err(e) = std::fs::remove_file(lock_path) {
                    tracing::warn!(
                        error = %e,
                        "[gpu-lock] Failed to remove lock file; it will become stale."
                    );
                } else {
                    tracing::info!(pid = owner_pid, "[gpu-lock] Lock released.");
                }
            }
        }
    }

    // ── Main acquire logic ────────────────────────────────────────────────────

    /// Acquire the interactive GPU lock.
    pub(super) fn acquire() -> Result<GpuLockGuardInner, GpuLockConflict> {
        let pid = std::process::id();

        tracing::info!("[gpu-lock] Checking {} ...", LOCK_FILE);

        // Ensure lock directory exists (non-fatal on failure).
        let lock_dir = Path::new(LOCK_DIR);
        if !lock_dir.exists() {
            tracing::info!("[gpu-lock] Creating lock directory: {LOCK_DIR}");
            if let Err(e) = std::fs::create_dir_all(lock_dir) {
                tracing::warn!(
                    error = %e,
                    "[gpu-lock] Could not create lock directory; proceeding without lock"
                );
                // Return a guard that will silently no-op on drop (file won't exist).
                return Ok(GpuLockGuardInner {
                    lock_path: PathBuf::from(LOCK_FILE),
                    owner_pid: pid,
                });
            }
        }

        let lock_path = Path::new(LOCK_FILE);

        // Check for an existing lock.
        if lock_path.exists() {
            match parse_lock_file(lock_path) {
                None => {
                    // Unreadable or corrupt — treat as absent. Log and continue.
                    tracing::warn!(
                        "[gpu-lock] Lock file exists but is unreadable/corrupt; treating as absent."
                    );
                    let _ = std::fs::remove_file(lock_path);
                }
                Some(fields) => {
                    let existing_pid: u32 =
                        fields.get("PID").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let existing_type = fields
                        .get("SESSION_TYPE")
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());
                    let existing_started = fields
                        .get("STARTED_AT")
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());
                    let existing_desc = fields.get("DESCRIPTION").cloned().unwrap_or_default();

                    tracing::info!(
                        session_type = %existing_type,
                        pid = existing_pid,
                        started_at = %existing_started,
                        "[gpu-lock] Lock file found: SESSION_TYPE={} PID={} STARTED_AT={}",
                        existing_type, existing_pid, existing_started,
                    );
                    if !existing_desc.is_empty() {
                        tracing::info!("[gpu-lock] Description: {}", existing_desc);
                    }

                    // A lock is only a live conflict if a genuinely-live
                    // tze_hud owns it. Boot-time is the primary defense: a lock
                    // predating the last boot is dead by definition (its writer
                    // could not have survived the reboot), which fixes the
                    // observed failure where a hard shutdown left the lock and a
                    // recycled PID read as "alive". The process-identity check is
                    // defense-in-depth for locks written since boot.
                    let boot_unix = system_boot_unix();
                    let live_facts = if existing_pid > 0 {
                        observe_process(existing_pid)
                    } else {
                        None
                    };

                    match lock_logic::classify_existing_lock(
                        &existing_started,
                        boot_unix,
                        live_facts.as_ref(),
                    ) {
                        LockClass::LiveConflict => {
                            // A live tze_hud owns the lock. Refuse startup.
                            tracing::error!(
                                existing_pid,
                                existing_session_type = %existing_type,
                                "[gpu-lock] tze_hud PID {} is alive and owns the lock. GPU is in use. Refusing startup.",
                                existing_pid
                            );
                            return Err(GpuLockConflict {
                                session_type: existing_type,
                                pid: existing_pid,
                                started_at: existing_started,
                                description: existing_desc,
                            });
                        }
                        LockClass::Stale => {
                            // Writer is gone, the lock predates boot, or the PID
                            // was recycled by an unrelated process — remove and
                            // take over.
                            let predates_boot = boot_unix
                                .map(|b| lock_logic::started_at_predates_boot(&existing_started, b))
                                .unwrap_or(false);
                            tracing::warn!(
                                stale_pid = existing_pid,
                                predates_boot,
                                "[gpu-lock] Lock is stale (writer gone, predates boot, or PID recycled). Removing and taking over."
                            );
                            if let Err(e) = std::fs::remove_file(lock_path) {
                                tracing::warn!(
                                    error = %e,
                                    "[gpu-lock] Could not remove stale lock; proceeding without lock"
                                );
                                return Ok(GpuLockGuardInner {
                                    lock_path: PathBuf::from(LOCK_FILE),
                                    owner_pid: pid,
                                });
                            }
                        }
                    }
                }
            }
        }

        if let Some(existing_pid) = find_existing_tze_hud_process(pid) {
            tracing::error!(
                existing_pid,
                "[gpu-lock] Found an already-running tze_hud.exe without a live lock. \
                 Refusing startup to avoid concurrent overlay/GPU ownership."
            );
            return Err(GpuLockConflict {
                session_type: "interactive-untracked".to_string(),
                pid: existing_pid,
                started_at: "unknown".to_string(),
                description: "existing tze_hud.exe process without a live gpu.lock".to_string(),
            });
        }

        // Acquire: write the interactive lock.
        let started_at = utc_now_rfc3339();
        let description = "tze_hud.exe interactive session";

        if write_lock("interactive", pid, &started_at, description) {
            tracing::info!(
                session_type = "interactive",
                pid,
                started_at = %started_at,
                "[gpu-lock] Lock acquired (SESSION_TYPE=interactive PID={} STARTED_AT={}).",
                pid, started_at,
            );
        }
        // Whether or not write succeeded, return the guard — it will only delete
        // the file on drop if it was actually written.

        // Install a panic hook so unwind also releases the lock.
        install_panic_hook(pid);

        Ok(GpuLockGuardInner {
            lock_path: PathBuf::from(LOCK_FILE),
            owner_pid: pid,
        })
    }

    // ── Panic hook ────────────────────────────────────────────────────────────

    /// Install a panic hook that releases the GPU lock before unwinding.
    ///
    /// This supplements Drop: if the runtime panics on a thread where the guard
    /// has not yet been dropped (or where panic = abort), the hook fires and
    /// deletes the lock file, preventing it from becoming a permanent stale lock.
    ///
    /// Idempotent: called once at acquire time.
    fn install_panic_hook(owner_pid: u32) {
        let lock_path = PathBuf::from(LOCK_FILE);
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!(
                "[gpu-lock] Panic detected — releasing GPU lock before unwind. {:?}",
                info
            );
            release_lock(&lock_path, owner_pid);
            prev_hook(info);
        }));
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// On non-Windows platforms, acquire must succeed and return None (no-op).
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn acquire_returns_none_on_non_windows() {
        let result = GpuLock::acquire();
        assert!(
            result.is_ok(),
            "acquire must not error on non-Windows: {result:?}"
        );
        assert!(
            result.unwrap().is_none(),
            "acquire must return None on non-Windows (no-op)"
        );
    }

    /// GpuLockConflict Display includes the session type and PID.
    #[test]
    fn gpu_lock_conflict_display_includes_pid_and_type() {
        let conflict = GpuLockConflict {
            session_type: "ci".to_string(),
            pid: 12345,
            started_at: "2026-04-25T18:00:00Z".to_string(),
            description: "nightly run".to_string(),
        };
        let s = conflict.to_string();
        assert!(s.contains("12345"), "Display should include PID: {s}");
        assert!(s.contains("ci"), "Display should include session type: {s}");
    }

    // ── Stale-lock classification (cross-platform pure logic) ─────────────────
    //
    // These exercise the reboot/PID-reuse fix without touching any Win32 API,
    // so they run deterministically on the Linux CI host. Boot time is injected
    // as a plain parameter, so tests never depend on the real system boot time.
    mod stale_lock {
        use super::super::lock_logic::{self, LockClass, ProcessFacts};

        #[test]
        fn parses_and_compares_started_at_against_boot() {
            let t = lock_logic::parse_rfc3339_utc("2026-07-03T17:48:04Z").unwrap();
            assert_eq!(t, lock_logic::civil_to_unix(2026, 7, 3, 17, 48, 4));

            let boot = lock_logic::civil_to_unix(2026, 7, 4, 0, 0, 0);
            // Written the day before boot → predates.
            assert!(lock_logic::started_at_predates_boot(
                "2026-07-03T17:48:04Z",
                boot
            ));
            // Written after boot → does not predate.
            assert!(!lock_logic::started_at_predates_boot(
                "2026-07-04T01:00:00Z",
                boot
            ));
            // Within the skew margin of boot → not treated as predating.
            assert!(!lock_logic::started_at_predates_boot(
                "2026-07-03T23:59:58Z",
                boot
            ));
            // Unparseable STARTED_AT never drives a reclaim on its own.
            assert!(!lock_logic::started_at_predates_boot("garbage", boot));
        }

        // Acceptance (a): a lock whose STARTED_AT predates the last boot is
        // reclaimed — the exact hud-7gp40 reproduction. Boot-time wins even when
        // a live-looking process occupies the PID.
        #[test]
        fn lock_predating_boot_is_reclaimed() {
            let boot = lock_logic::civil_to_unix(2026, 7, 4, 0, 0, 0);
            let facts = ProcessFacts {
                image_is_tze_hud: true,
                creation_unix: Some(boot + 10),
            };
            assert!(matches!(
                lock_logic::classify_existing_lock(
                    "2026-07-03T17:48:04Z",
                    Some(boot),
                    Some(&facts),
                ),
                LockClass::Stale
            ));
        }

        // Acceptance (b): a genuinely-live tze_hud holding the lock still blocks
        // a second startup — mutual exclusion is preserved.
        #[test]
        fn live_tze_hud_holder_blocks_startup() {
            let boot = lock_logic::civil_to_unix(2026, 7, 3, 0, 0, 0);
            let started = "2026-07-03T10:00:00Z";
            // Creation ~2s before the lock write — a matching author.
            let created = lock_logic::civil_to_unix(2026, 7, 3, 9, 59, 58);
            let facts = ProcessFacts {
                image_is_tze_hud: true,
                creation_unix: Some(created),
            };
            assert!(matches!(
                lock_logic::classify_existing_lock(started, Some(boot), Some(&facts)),
                LockClass::LiveConflict
            ));
        }

        // Acceptance (c): a recycled PID owned by an unrelated (non-tze_hud)
        // process does NOT keep startup refused.
        #[test]
        fn recycled_pid_of_unrelated_process_is_reclaimed() {
            let boot = lock_logic::civil_to_unix(2026, 7, 3, 0, 0, 0);
            let started = "2026-07-03T10:00:00Z"; // after boot; boot check won't fire
            let facts = ProcessFacts {
                image_is_tze_hud: false,
                creation_unix: Some(boot + 42),
            };
            assert!(matches!(
                lock_logic::classify_existing_lock(started, Some(boot), Some(&facts)),
                LockClass::Stale
            ));
        }

        // A different tze_hud that reused the PID (creation time far from
        // STARTED_AT) is not the lock's author → reclaim. (A concurrently-live
        // untracked tze_hud is separately caught by find_existing_tze_hud_process.)
        #[test]
        fn tze_hud_pid_with_mismatched_creation_is_reclaimed() {
            let boot = lock_logic::civil_to_unix(2026, 7, 3, 0, 0, 0);
            let started = "2026-07-03T10:00:00Z";
            let created = lock_logic::civil_to_unix(2026, 7, 3, 12, 0, 0); // 2h later
            let facts = ProcessFacts {
                image_is_tze_hud: true,
                creation_unix: Some(created),
            };
            assert!(matches!(
                lock_logic::classify_existing_lock(started, Some(boot), Some(&facts)),
                LockClass::Stale
            ));
        }

        // A dead PID (no live process) with a post-boot STARTED_AT is stale.
        #[test]
        fn dead_pid_after_boot_is_reclaimed() {
            let boot = lock_logic::civil_to_unix(2026, 7, 3, 0, 0, 0);
            assert!(matches!(
                lock_logic::classify_existing_lock("2026-07-03T10:00:00Z", Some(boot), None),
                LockClass::Stale
            ));
        }

        // With no derivable boot time, classification falls back to the
        // process-identity check: a live tze_hud author still blocks.
        #[test]
        fn falls_back_to_identity_when_boot_unknown() {
            let started = "2026-07-03T10:00:00Z";
            let created = lock_logic::civil_to_unix(2026, 7, 3, 10, 0, 0);
            let live = ProcessFacts {
                image_is_tze_hud: true,
                creation_unix: Some(created),
            };
            assert!(matches!(
                lock_logic::classify_existing_lock(started, None, Some(&live)),
                LockClass::LiveConflict
            ));
            // …and a recycled non-tze_hud PID is still reclaimed.
            let recycled = ProcessFacts {
                image_is_tze_hud: false,
                creation_unix: None,
            };
            assert!(matches!(
                lock_logic::classify_existing_lock(started, None, Some(&recycled)),
                LockClass::Stale
            ));
        }
    }
}
