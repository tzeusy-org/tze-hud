//! Signed pull-only self-update behind `POST /admin/update`.
//!
//! The runtime downloads `tze_hud.exe` and `tze_hud.exe.minisig` for a channel
//! with the system `curl.exe`, verifies the minisign signature against the
//! public key compiled into this binary, and only then swaps files and hands
//! over to the new exe (see [`super::handoff`]). A failed handoff puts the old
//! exe back. Nothing in a request chooses a URL, a path or an argument: the
//! request names a channel, which is validated against a closed grammar.
//!
//! Trust model: the signature binds the exe bytes and the trusted comment
//! `tze_hud <ref> <sha>`. The ref must match the requested channel, so a
//! validly signed older build cannot be replayed as another channel's
//! latest, and an identical sha is reported as already up to date.
//!
//! Failure modes are deliberately constant on the wire (`UPDATE_FAILED`, one
//! hint): which check rejected a download is logged, not returned.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use minisign_verify::{PublicKey, Signature};
use serde_json::{Value, json};

use super::handoff::{self, ChildProc, ChildSpec, Quit};
use super::install::{InstallPaths, same_path};

/// The release signing public key (the file the release workflow verifies
/// against). Rotating the key means changing this one file.
pub const PUBLIC_KEY: &str = include_str!("../../../../app/tze_hud_app/minisign.pub");

/// Release downloads for the repository; `TZE_HUD_RELEASES_URL` overrides it.
pub const DEFAULT_RELEASES_URL: &str = "https://github.com/tzeusy-org/tze-hud/releases";
pub const RELEASES_URL_ENV: &str = "TZE_HUD_RELEASES_URL";
/// Whole-download budget per file.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

const MAX_EXE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SIG_BYTES: u64 = 4096;
const STAGED_EXE: &str = "tze_hud.update.exe";
const STAGED_SIG: &str = "tze_hud.update.minisig";
/// Constant failure text for the wire and `last_update.error`.
const FAILED_HINT: &str = "update failed; the running version is unchanged (see the runtime log)";

// ── Channel ──────────────────────────────────────────────────────────────────

/// What `POST /admin/update` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Channel {
    /// The rolling build of `main`.
    Dev,
    /// The latest tagged release.
    Stable,
    /// One tagged release, `v<digit>[0-9A-Za-z.-]*`.
    Tag(String),
}

impl Channel {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dev" => Some(Self::Dev),
            "stable" => Some(Self::Stable),
            _ if is_tag(s) => Some(Self::Tag(s.to_owned())),
            _ => None,
        }
    }

    /// `https://.../releases/...` for one release asset.
    fn url(&self, base: &str, file: &str) -> String {
        match self {
            Self::Dev => format!("{base}/download/dev/{file}"),
            Self::Stable => format!("{base}/latest/download/{file}"),
            Self::Tag(tag) => format!("{base}/download/{tag}/{file}"),
        }
    }

    /// Does the signed ref belong to this channel? `stable` has no fixed ref:
    /// any tag does, but never `dev`.
    fn accepts_ref(&self, signed_ref: &str) -> bool {
        match self {
            Self::Dev => signed_ref == "dev",
            Self::Stable => is_tag(signed_ref),
            Self::Tag(tag) => signed_ref == tag,
        }
    }
}

fn is_tag(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('v') else {
        return false;
    };
    s.len() <= 32
        && rest.starts_with(|c: char| c.is_ascii_digit())
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

// ── Fetch ────────────────────────────────────────────────────────────────────

/// How downloads are made. Injectable so tests use a local server and a short
/// timeout.
#[derive(Debug, Clone)]
pub struct UpdateConfig {
    /// Without a trailing slash.
    pub base_url: String,
    /// The curl executable.
    pub curl: PathBuf,
    pub fetch_timeout: Duration,
}

impl UpdateConfig {
    pub fn from_env() -> Self {
        let base = std::env::var(RELEASES_URL_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_RELEASES_URL.to_owned());
        Self {
            base_url: base.trim_end_matches('/').to_owned(),
            curl: system_curl(),
            fetch_timeout: FETCH_TIMEOUT,
        }
    }
}

/// `%SystemRoot%\System32\curl.exe` (an absolute path, so neither PATH nor the
/// working directory can substitute another program).
fn system_curl() -> PathBuf {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        Path::new(&root).join("System32").join("curl.exe")
    } else {
        PathBuf::from("curl")
    }
}

/// The curl argument vector for one download (passed as an args vector, never
/// through a shell). Only the scheme of `url` is allowed, including across
/// redirects when it is https.
fn curl_args(url: &str, out: &Path, timeout: Duration, max_bytes: u64) -> Vec<std::ffi::OsString> {
    let https = url.starts_with("https://");
    let mut args: Vec<std::ffi::OsString> = vec![
        "-fsSL".into(),
        "--max-time".into(),
        timeout.as_secs().max(1).to_string().into(),
        "--max-filesize".into(),
        max_bytes.to_string().into(),
        "--proto".into(),
        if https { "=https" } else { "=http" }.into(),
    ];
    if https {
        args.extend(["--proto-redir".into(), "=https".into()]);
    }
    args.extend([
        "-o".into(),
        out.as_os_str().to_owned(),
        "--url".into(),
        url.into(),
    ]);
    args
}

fn fetch(cfg: &UpdateConfig, url: &str, out: &Path, max_bytes: u64) -> Result<(), Failure> {
    let mut cmd = Command::new(&cfg.curl);
    cmd.args(curl_args(url, out, cfg.fetch_timeout, max_bytes))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;
        cmd.creation_flags(CREATE_NO_WINDOW.0);
    }
    let mut child = cmd.spawn().map_err(|_| Failure::Fetch)?;
    // curl enforces `--max-time`; the extra margin only guards against a hang.
    let deadline = Instant::now() + cfg.fetch_timeout + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) | Err(_) => return Err(Failure::Fetch),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Fetch);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

// ── Verify ───────────────────────────────────────────────────────────────────

/// A release that passed signature and channel checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub signed_ref: String,
    pub sha: String,
}

/// Why an update stopped. Logged; never sent to the client.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Fetch,
    /// Bad key, bad signature format, or the signature does not match.
    Signature,
    /// Signed, but the trusted comment is malformed or names another channel.
    Channel,
    Io(&'static str),
    Handoff(String),
}

/// Check `exe` against `sig_text` with `public_key`, then the trusted comment
/// against `channel`. The exe is hashed from disk in chunks.
pub fn verify(
    public_key: &str,
    exe: &Path,
    sig_text: &str,
    channel: &Channel,
) -> Result<Release, Failure> {
    let key = PublicKey::decode(public_key).map_err(|_| Failure::Signature)?;
    let sig = Signature::decode(sig_text).map_err(|_| Failure::Signature)?;
    let mut verifier = key.verify_stream(&sig).map_err(|_| Failure::Signature)?;
    let mut file = File::open(exe).map_err(|_| Failure::Io("open downloaded exe"))?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|_| Failure::Io("read downloaded exe"))?;
        if n == 0 {
            break;
        }
        verifier.update(&buf[..n]);
    }
    verifier.finalize().map_err(|_| Failure::Signature)?;
    parse_trusted_comment(sig.trusted_comment(), channel)
}

/// `tze_hud <ref> <40 lowercase hex sha>`, with `<ref>` valid for `channel`.
fn parse_trusted_comment(comment: &str, channel: &Channel) -> Result<Release, Failure> {
    let mut parts = comment.split(' ');
    let (Some("tze_hud"), Some(signed_ref), Some(sha), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Failure::Channel);
    };
    let sha_ok = sha.len() == 40 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !sha_ok || !channel.accepts_ref(signed_ref) {
        return Err(Failure::Channel);
    }
    Ok(Release {
        signed_ref: signed_ref.to_owned(),
        sha: sha.to_owned(),
    })
}

// ── Stage ────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum Staged {
    /// The release is the running build; nothing was kept.
    UpToDate,
    /// The verified exe sits at `<install dir>\tze_hud.update.exe`.
    Ready(Release),
}

fn staged_paths(paths: &InstallPaths) -> (PathBuf, PathBuf) {
    (
        paths.install_dir.join(STAGED_EXE),
        paths.install_dir.join(STAGED_SIG),
    )
}

fn discard_staged(paths: &InstallPaths) {
    let (exe, sig) = staged_paths(paths);
    let _ = fs::remove_file(exe);
    let _ = fs::remove_file(sig);
}

/// Download, verify and stage the channel's exe in the install dir (same
/// volume as the target, so the swap is a rename). Leaves nothing behind on
/// any outcome but [`Staged::Ready`], and never touches `tze_hud.exe`.
pub fn stage(
    cfg: &UpdateConfig,
    public_key: &str,
    paths: &InstallPaths,
    channel: &Channel,
    current_sha: &str,
) -> Result<Staged, Failure> {
    let (exe, sig) = staged_paths(paths);
    discard_staged(paths);
    let result = (|| {
        fetch(
            cfg,
            &channel.url(&cfg.base_url, "tze_hud.exe"),
            &exe,
            MAX_EXE_BYTES,
        )?;
        fetch(
            cfg,
            &channel.url(&cfg.base_url, "tze_hud.exe.minisig"),
            &sig,
            MAX_SIG_BYTES,
        )?;
        let sig_text = fs::read_to_string(&sig).map_err(|_| Failure::Signature)?;
        verify(public_key, &exe, &sig_text, channel)
    })();
    match result {
        Ok(release) if release.sha == current_sha => {
            discard_staged(paths);
            Ok(Staged::UpToDate)
        }
        Ok(release) => {
            let _ = fs::remove_file(sig);
            Ok(Staged::Ready(release))
        }
        Err(e) => {
            discard_staged(paths);
            Err(e)
        }
    }
}

// ── Swap ─────────────────────────────────────────────────────────────────────

/// Park the running exe as `tze_hud.old.exe`, move the staged exe into place
/// and run `handoff`. If anything fails, `tze_hud.exe` is the original again.
pub fn swap_in(
    paths: &InstallPaths,
    handoff: impl FnOnce() -> Result<(), String>,
) -> Result<(), Failure> {
    let (staged, _) = staged_paths(paths);
    if !staged.is_file() {
        return Err(Failure::Io("staged exe missing"));
    }
    match fs::remove_file(&paths.old_exe) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(Failure::Io("clear tze_hud.old.exe")),
    }
    fs::rename(&paths.exe, &paths.old_exe).map_err(|_| Failure::Io("park tze_hud.exe"))?;
    if fs::rename(&staged, &paths.exe).is_err() {
        restore(paths);
        discard_staged(paths);
        return Err(Failure::Io("move staged exe into place"));
    }
    // A panicking handoff must not leave the unproven exe in place.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(handoff))
        .unwrap_or_else(|_| Err("handoff panicked".to_owned()));
    if let Err(why) = outcome {
        restore(paths);
        return Err(Failure::Handoff(why));
    }
    Ok(())
}

/// Put `tze_hud.old.exe` back as `tze_hud.exe`, dropping whatever is there.
fn restore(paths: &InstallPaths) {
    let _ = fs::remove_file(&paths.exe);
    if let Err(e) = fs::rename(&paths.old_exe, &paths.exe) {
        tracing::error!(error = %e, "update rollback could not restore tze_hud.exe");
    }
}

// ── Handle ───────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum UpdateError {
    /// The body did not name a valid channel.
    BadChannel,
    /// Not running from the install path, so there is nothing to replace.
    NotInstalled,
    /// An update is already in progress.
    Busy,
    /// Constant on the wire; the cause is in the log.
    Failed,
    /// The update thread could not be started.
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    /// Verified; the swap and handoff continue in the background.
    Started {
        sha: String,
    },
}

impl UpdateError {
    pub fn hint(&self) -> &'static str {
        match self {
            Self::BadChannel => "send {\"channel\":\"dev\"|\"stable\"|\"v1.2.3\"}",
            Self::NotInstalled => "run tze_hud.exe --install, then update the installed copy",
            Self::Busy => "an update is already in progress",
            Self::Failed => FAILED_HINT,
            Self::Unavailable => "could not start the update",
        }
    }
}

type Spawner = dyn Fn(&ChildSpec) -> io::Result<Box<dyn ChildProc>> + Send + Sync;
type Notify = dyn Fn(String) + Send + Sync;

struct Inner {
    cfg: UpdateConfig,
    public_key: String,
    paths: InstallPaths,
    current_exe: PathBuf,
    current_sha: String,
    /// `dev-1a2b3c4`: what this build is called in toasts.
    current_label: String,
    spawn: Box<Spawner>,
    quit: Quit,
    timeout: Duration,
    notify: Box<Notify>,
    busy: AtomicBool,
    last: Mutex<Option<Result<String, ()>>>,
}

/// Clears the busy flag when dropped, so an early return or a panic cannot
/// leave updates (and restarts' sibling flag) wedged. [`Self::keep`] is for the
/// one case where this instance is about to exit.
struct BusyGuard {
    inner: Arc<Inner>,
    keep: bool,
}

impl BusyGuard {
    fn acquire(inner: &Arc<Inner>) -> Option<Self> {
        if inner.busy.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(Self {
            inner: Arc::clone(inner),
            keep: false,
        })
    }

    fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if !self.keep {
            self.inner.busy.store(false, Ordering::Release);
        }
    }
}

/// Self-update for the installed exe. One update at a time; cheap to clone.
#[derive(Clone)]
pub struct UpdateHandle(Arc<Inner>);

impl std::fmt::Debug for UpdateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateHandle").finish_non_exhaustive()
    }
}

/// Toast label for a build: `<channel>-<sha7>`.
pub fn label(channel: &str, sha: &str) -> String {
    format!("{channel}-{}", sha.get(..7).unwrap_or(sha))
}

impl UpdateHandle {
    /// Update `paths.exe`, which must be `current_exe`. The new instance gets
    /// `original_args` plus `--updated-from <current_sha>`. `notify` shows a
    /// toast.
    pub fn new(
        paths: InstallPaths,
        current_exe: PathBuf,
        original_args: Vec<String>,
        current_sha: String,
        current_label: String,
        quit: Quit,
        notify: Box<Notify>,
    ) -> Self {
        let exe = paths.exe.clone();
        let extra = ["--updated-from".to_owned(), current_sha.clone()];
        Self::with_parts(
            UpdateConfig::from_env(),
            PUBLIC_KEY.to_owned(),
            paths,
            current_exe,
            current_sha,
            current_label,
            Box::new(move |spec| handoff::spawn_child_with(&exe, &original_args, &extra, spec)),
            quit,
            handoff::HANDOFF_TIMEOUT,
            notify,
        )
    }

    #[expect(clippy::too_many_arguments, reason = "every part is injectable")]
    pub fn with_parts(
        cfg: UpdateConfig,
        public_key: String,
        paths: InstallPaths,
        current_exe: PathBuf,
        current_sha: String,
        current_label: String,
        spawn: Box<Spawner>,
        quit: Quit,
        timeout: Duration,
        notify: Box<Notify>,
    ) -> Self {
        Self(Arc::new(Inner {
            cfg,
            public_key,
            paths,
            current_exe,
            current_sha,
            current_label,
            spawn,
            quit,
            timeout,
            notify,
            busy: AtomicBool::new(false),
            last: Mutex::new(None),
        }))
    }

    /// Download and verify the channel's release (blocking: call from a
    /// blocking context). `UpToDate` or `Started` once verified; the swap and
    /// handoff then run on their own thread, and their outcome is in
    /// [`Self::last_json`] and a toast.
    pub fn request(&self, channel: &str) -> Result<Outcome, UpdateError> {
        let inner = &self.0;
        let channel = Channel::parse(channel).ok_or(UpdateError::BadChannel)?;
        if !same_path(&inner.current_exe, &inner.paths.exe) {
            return Err(UpdateError::NotInstalled);
        }
        let guard = BusyGuard::acquire(inner).ok_or(UpdateError::Busy)?;
        let staged = stage(
            &inner.cfg,
            &inner.public_key,
            &inner.paths,
            &channel,
            &inner.current_sha,
        );
        let release = match staged {
            Ok(Staged::UpToDate) => return Ok(Outcome::UpToDate),
            Ok(Staged::Ready(release)) => release,
            Err(why) => {
                inner.record_failure(&why);
                return Err(UpdateError::Failed);
            }
        };
        let sha = release.sha.clone();
        let worker = Arc::clone(inner);
        std::thread::Builder::new()
            .name("update".into())
            .spawn(move || worker.swap_and_handoff(release, guard))
            .map_err(|_| {
                discard_staged(&inner.paths);
                UpdateError::Unavailable
            })?;
        Ok(Outcome::Started { sha })
    }

    /// `last_update` for `/admin/status`: `null` until an update was
    /// attempted, else `{ok, sha, error}`.
    pub fn last_json(&self) -> Value {
        match &*self.0.last.lock().unwrap_or_else(|e| e.into_inner()) {
            None => Value::Null,
            Some(Ok(sha)) => json!({"ok": true, "sha": sha, "error": Value::Null}),
            Some(Err(())) => json!({"ok": false, "sha": Value::Null, "error": FAILED_HINT}),
        }
    }
}

impl Inner {
    fn record_failure(&self, why: &Failure) {
        tracing::error!(cause = ?why, "update failed; still on the running version");
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(()));
        (self.notify)(format!("Update failed; still on {}", self.current_label));
    }

    fn swap_and_handoff(&self, release: Release, guard: BusyGuard) {
        let quit = Arc::clone(&self.quit);
        let result = swap_in(&self.paths, || {
            handoff::handoff(|spec| (self.spawn)(spec), self.timeout, move || quit())
                .map(|ready| {
                    tracing::info!(
                        pid = ready.pid,
                        "update: new instance is ready; shutting down"
                    )
                })
                .map_err(|e| e.to_string())
        });
        match result {
            Ok(()) => {
                *self.last.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(Ok(release.sha.clone()));
                // This instance is exiting; stay busy.
                guard.keep();
            }
            Err(why) => self.record_failure(&why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    // A throwaway keypair (secret discarded) and signatures over `PAYLOAD`,
    // made with the minisign crate's `sign` (prehashed, as the release
    // workflow's `minisign -S`). Trusted comments: see each constant.
    const TEST_KEY: &str = "untrusted comment: minisign public key: 1E9D7823BA137FBA\nRWS6fxO6I3idHv7AazvgTWkNbwME6+/ecxb8TYKvZqCbqJmln6oOfERL\n";
    const PAYLOAD: &[u8] = b"MZ tze_hud test payload\n";
    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    /// `tze_hud dev <SHA_A>`
    const SIG_DEV_A: &str = "untrusted comment: test\nRUS6fxO6I3idHp2ojHX/xjvdOGtZ/fdGvb0lfl8st8iZq7AIluyX7epO8JFm/AGSMV5CJYT4AF9U9kEV6wSL1cxFD/C3StoUugs=\ntrusted comment: tze_hud dev aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nSMdzBGuNaP5svS872fvWfZkj8N1KNlqEGVdi0RtI6QXax6DfiZMgGHkq9rhZm0JHlWKEl8oM60JbrMQigBRMCA==\n";
    /// `tze_hud main <SHA_A>` (what the release workflow used to sign)
    const SIG_MAIN_A: &str = "untrusted comment: test\nRUS6fxO6I3idHkrcdpBD1d6ZvhPJ6SzEZxQbaaD3CUeTr0pKUAtrc/SU3mHi2vemFtUZ2mV1IzPvZnylDh5C8z4F+bd1IbSaowE=\ntrusted comment: tze_hud main aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbtUEX9HEVumzhg1qegDYGfDt+NsP5hXVyR/3CtAr+EvK8EnKjKWxPvv+ol0D6XwJEnY31F64JoFh4YfMWdJ6Bw==\n";
    /// `tze_hud v1.2.3 bbbb...b`
    const SIG_TAG_B: &str = "untrusted comment: test\nRUS6fxO6I3idHtdeWRontPKWtCFJfaqwDXh9vDgiwQ4wdnoiTcAMXCXC/Zryy8QG/jv+Muz7IcdK6e4X3m9A16fI8dPdI8JlBgQ=\ntrusted comment: tze_hud v1.2.3 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n+XqHnliCXpdGgi5Inu/qSvlWmq/KS/xackhGMhWoNbO4kAL/4tT2NarWAx6oOVRWBxLH+PmZ2Ikeb08tSJRICQ==\n";
    /// `tze_hud dev <SHA_A>`, signed by a different key
    const SIG_OTHER_KEY: &str = "untrusted comment: test\nRUTlbrSBtkcBNPh/2TGCX3+BXfFU7m18kLiB0azWllODMRYEt8LYlRu8CEkb5cXCh9zMm6wnFqbCy6a0AQuQIsjJZQ1DiVXyuwE=\ntrusted comment: tze_hud dev aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nriaXodszhU3w921k5m5SCUgo7mdih/ZU3oPYyElz+ZVqnC0WMLQ9ZX4OffzqmJm4Y/K/vZYLlb6Y5s2QMjT0Bg==\n";
    const OLD_EXE: &[u8] = b"MZ the old running exe";

    /// Serves `files` over HTTP on loopback until dropped; records request paths.
    struct Server {
        base: String,
        seen: Arc<Mutex<Vec<String>>>,
    }

    fn serve(files: Vec<(&'static str, Vec<u8>)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/releases", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for mut conn in listener.incoming().flatten() {
                let mut buf = [0u8; 2048];
                let n = conn.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = head.split(' ').nth(1).unwrap_or("").to_owned();
                log.lock().unwrap().push(path.clone());
                let reply = match files.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(body);
                        r
                    }
                    None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                };
                let _ = conn.write_all(&reply);
            }
        });
        Server { base, seen }
    }

    fn release_files(prefix: &'static str, exe: &[u8], sig: &str) -> Vec<(&'static str, Vec<u8>)> {
        let exe_path: &'static str = Box::leak(format!("{prefix}/tze_hud.exe").into_boxed_str());
        let sig_path: &'static str =
            Box::leak(format!("{prefix}/tze_hud.exe.minisig").into_boxed_str());
        vec![
            (exe_path, exe.to_vec()),
            (sig_path, sig.as_bytes().to_vec()),
        ]
    }

    fn installed(tag: &str) -> InstallPaths {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("tze_hud_update_{tag}{n}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let paths = InstallPaths::from_dirs(&root.join("Local"), &root.join("Roaming"));
        fs::create_dir_all(&paths.install_dir).unwrap();
        fs::write(&paths.exe, OLD_EXE).unwrap();
        paths
    }

    fn cfg(base: &str) -> UpdateConfig {
        UpdateConfig {
            base_url: base.to_owned(),
            curl: PathBuf::from("curl"),
            fetch_timeout: Duration::from_secs(10),
        }
    }

    fn dir_listing(paths: &InstallPaths) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(&paths.install_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn stage_dev(sig: &str, exe: &[u8], current: &str) -> (Result<Staged, Failure>, InstallPaths) {
        let server = serve(release_files("/releases/download/dev", exe, sig));
        let paths = installed("stage");
        let r = stage(&cfg(&server.base), TEST_KEY, &paths, &Channel::Dev, current);
        (r, paths)
    }

    #[test]
    fn valid_signature_is_staged_beside_the_exe_and_nothing_is_swapped() {
        let server = serve(release_files("/releases/download/dev", PAYLOAD, SIG_DEV_A));
        let paths = installed("valid");
        let staged = stage(&cfg(&server.base), TEST_KEY, &paths, &Channel::Dev, "old").unwrap();
        assert_eq!(
            staged,
            Staged::Ready(Release {
                signed_ref: "dev".into(),
                sha: SHA_A.into()
            })
        );
        assert_eq!(
            fs::read(paths.install_dir.join(STAGED_EXE)).unwrap(),
            PAYLOAD
        );
        assert_eq!(fs::read(&paths.exe).unwrap(), OLD_EXE);
        assert!(!paths.old_exe.exists());
        assert_eq!(
            *server.seen.lock().unwrap(),
            [
                "/releases/download/dev/tze_hud.exe",
                "/releases/download/dev/tze_hud.exe.minisig"
            ]
        );
    }

    #[test]
    fn tampered_exe_or_signature_is_rejected_and_nothing_is_kept() {
        let mut flipped = PAYLOAD.to_vec();
        flipped[3] ^= 1;
        let forged_comment = SIG_DEV_A.replace("tze_hud dev aaaa", "tze_hud dev bbbb");
        let cases = [
            (SIG_DEV_A, flipped, "one exe byte flipped"),
            (SIG_DEV_A, b"MZ".to_vec(), "truncated exe"),
            (
                &forged_comment[..],
                PAYLOAD.to_vec(),
                "trusted comment edited",
            ),
            (SIG_OTHER_KEY, PAYLOAD.to_vec(), "signed by another key"),
            ("not a signature", PAYLOAD.to_vec(), "garbage signature"),
        ];
        for (sig, exe, what) in cases {
            let (r, paths) = stage_dev(sig, &exe, "old");
            assert_eq!(r, Err(Failure::Signature), "{what}");
            assert_eq!(dir_listing(&paths), ["tze_hud.exe"], "{what}");
            assert_eq!(fs::read(&paths.exe).unwrap(), OLD_EXE, "{what}");
        }
    }

    #[test]
    fn a_missing_download_fails_without_residue() {
        let server = serve(vec![]);
        let paths = installed("missing");
        let r = stage(&cfg(&server.base), TEST_KEY, &paths, &Channel::Dev, "old");
        assert_eq!(r, Err(Failure::Fetch));
        assert_eq!(dir_listing(&paths), ["tze_hud.exe"]);
    }

    #[test]
    fn the_signed_ref_must_belong_to_the_requested_channel() {
        // Validly signed, but by a `main` build: a dev request must refuse it.
        let (r, paths) = stage_dev(SIG_MAIN_A, PAYLOAD, "old");
        assert_eq!(r, Err(Failure::Channel));
        assert_eq!(dir_listing(&paths), ["tze_hud.exe"]);

        let ok = |channel: &Channel, sig: &str| {
            let dir = std::env::temp_dir().join(format!("tze_hud_verify_{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let exe = dir.join("x.exe");
            fs::write(&exe, PAYLOAD).unwrap();
            verify(TEST_KEY, &exe, sig, channel)
        };
        let tag = |t: &str| Channel::Tag(t.into());
        assert!(ok(&Channel::Dev, SIG_DEV_A).is_ok());
        assert!(ok(&tag("v1.2.3"), SIG_TAG_B).is_ok());
        assert!(ok(&Channel::Stable, SIG_TAG_B).is_ok());
        assert_eq!(ok(&tag("v9.9.9"), SIG_TAG_B), Err(Failure::Channel));
        assert_eq!(ok(&Channel::Dev, SIG_TAG_B), Err(Failure::Channel));
        assert_eq!(ok(&Channel::Stable, SIG_DEV_A), Err(Failure::Channel));
        assert_eq!(ok(&tag("v1.2.3"), SIG_DEV_A), Err(Failure::Channel));
    }

    #[test]
    fn trusted_comment_grammar_is_strict() {
        let sha = SHA_A;
        let good = format!("tze_hud dev {sha}");
        assert!(parse_trusted_comment(&good, &Channel::Dev).is_ok());
        for bad in [
            format!("tze_hud dev {sha} extra"),
            format!("tze_hud dev  {sha}"),
            format!("other dev {sha}"),
            format!("tze_hud dev {}", sha.to_uppercase()),
            format!("tze_hud dev {}", &sha[..39]),
            "tze_hud dev".to_owned(),
            String::new(),
        ] {
            assert_eq!(
                parse_trusted_comment(&bad, &Channel::Dev),
                Err(Failure::Channel),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_running_sha_is_up_to_date_and_leaves_nothing() {
        let (r, paths) = stage_dev(SIG_DEV_A, PAYLOAD, SHA_A);
        assert_eq!(r, Ok(Staged::UpToDate));
        assert_eq!(dir_listing(&paths), ["tze_hud.exe"]);
    }

    #[test]
    fn channels_parse_a_closed_grammar_and_map_to_release_urls() {
        assert_eq!(Channel::parse("dev"), Some(Channel::Dev));
        assert_eq!(Channel::parse("stable"), Some(Channel::Stable));
        assert_eq!(
            Channel::parse("v1.2.3"),
            Some(Channel::Tag("v1.2.3".into()))
        );
        for bad in [
            "",
            "main",
            "Dev",
            "v",
            "vx",
            "v1/../x",
            "v1 2",
            "v1?x",
            "-o",
            "v1\n",
            "v1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert_eq!(Channel::parse(bad), None, "{bad:?}");
        }
        let base = "https://h/releases";
        assert_eq!(
            Channel::Dev.url(base, "tze_hud.exe"),
            "https://h/releases/download/dev/tze_hud.exe"
        );
        assert_eq!(
            Channel::Stable.url(base, "tze_hud.exe.minisig"),
            "https://h/releases/latest/download/tze_hud.exe.minisig"
        );
        assert_eq!(
            Channel::Tag("v1.2.3".into()).url(base, "tze_hud.exe"),
            "https://h/releases/download/v1.2.3/tze_hud.exe"
        );
    }

    #[test]
    fn curl_args_are_a_fixed_vector_that_pins_scheme_and_sizes() {
        let args = |url: &str| -> Vec<String> {
            curl_args(url, Path::new("/i/out"), Duration::from_secs(120), 99)
                .into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            args("https://h/x"),
            [
                "-fsSL",
                "--max-time",
                "120",
                "--max-filesize",
                "99",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "-o",
                "/i/out",
                "--url",
                "https://h/x"
            ]
        );
        assert_eq!(args("http://127.0.0.1:1/x")[5..7], ["--proto", "=http"]);
    }

    #[test]
    fn swap_replaces_the_exe_and_a_failed_handoff_restores_it_byte_for_byte() {
        let stage_new = |paths: &InstallPaths| fs::write(staged_paths(paths).0, PAYLOAD).unwrap();

        let paths = installed("swap_ok");
        stage_new(&paths);
        swap_in(&paths, || Ok(())).unwrap();
        assert_eq!(fs::read(&paths.exe).unwrap(), PAYLOAD);
        assert_eq!(fs::read(&paths.old_exe).unwrap(), OLD_EXE);

        let paths = installed("swap_rollback");
        fs::write(&paths.old_exe, b"stale previous upgrade").unwrap();
        stage_new(&paths);
        let seen_during = Mutex::new(Vec::new());
        let r = swap_in(&paths, || {
            *seen_during.lock().unwrap() = fs::read(&paths.exe).unwrap();
            Err("new instance never reported ready".into())
        });
        assert!(matches!(r, Err(Failure::Handoff(_))));
        // The handoff ran against the new exe, then the old one came back.
        assert_eq!(*seen_during.lock().unwrap(), PAYLOAD);
        assert_eq!(fs::read(&paths.exe).unwrap(), OLD_EXE);
        assert_eq!(dir_listing(&paths), ["tze_hud.exe"]);

        // A panicking handoff is a failed handoff.
        let paths = installed("swap_panic");
        stage_new(&paths);
        let r = swap_in(&paths, || panic!("boom"));
        assert!(matches!(r, Err(Failure::Handoff(_))));
        assert_eq!(fs::read(&paths.exe).unwrap(), OLD_EXE);

        // No staged exe: nothing is renamed.
        let paths = installed("swap_none");
        assert!(matches!(swap_in(&paths, || Ok(())), Err(Failure::Io(_))));
        assert_eq!(dir_listing(&paths), ["tze_hud.exe"]);
    }

    struct Fake(Option<String>);
    impl ChildProc for Fake {
        fn try_wait(&mut self) -> io::Result<Option<String>> {
            Ok(self.0.clone())
        }
        fn kill(&mut self) {}
    }

    struct Rig {
        handle: UpdateHandle,
        paths: InstallPaths,
        toasts: Arc<Mutex<Vec<String>>>,
        _server: Server,
    }

    fn rig(tag: &str, spawn: Box<Spawner>) -> Rig {
        let server = serve(release_files("/releases/download/dev", PAYLOAD, SIG_DEV_A));
        let paths = installed(tag);
        let toasts = Arc::new(Mutex::new(Vec::new()));
        let t = Arc::clone(&toasts);
        let handle = UpdateHandle::with_parts(
            cfg(&server.base),
            TEST_KEY.to_owned(),
            paths.clone(),
            paths.exe.clone(),
            "old".into(),
            "dev-oldsha1".into(),
            spawn,
            Arc::new(|| {}),
            Duration::from_millis(300),
            Box::new(move |s| t.lock().unwrap().push(s)),
        );
        Rig {
            handle,
            paths,
            toasts,
            _server: server,
        }
    }

    fn wait_until(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_failed_handoff_rolls_back_toasts_and_frees_the_next_attempt() {
        let rig = rig(
            "handle_rollback",
            Box::new(|_| Ok(Box::new(Fake(Some("exit 3".into()))))),
        );
        assert_eq!(rig.handle.last_json(), Value::Null);
        assert_eq!(
            rig.handle.request("dev"),
            Ok(Outcome::Started { sha: SHA_A.into() })
        );
        wait_until("rollback", || !rig.toasts.lock().unwrap().is_empty());
        assert_eq!(
            *rig.toasts.lock().unwrap(),
            ["Update failed; still on dev-oldsha1"]
        );
        assert_eq!(fs::read(&rig.paths.exe).unwrap(), OLD_EXE);
        assert_eq!(rig.handle.last_json()["ok"], false);
        assert_eq!(rig.handle.last_json()["error"], FAILED_HINT);
        // Not wedged: another attempt is admitted (and fails the same way).
        assert_ne!(rig.handle.request("dev"), Err(UpdateError::Busy));
    }

    #[test]
    fn a_ready_handoff_keeps_the_new_exe_and_stays_busy() {
        let quits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let q = Arc::clone(&quits);
        let server = serve(release_files("/releases/download/dev", PAYLOAD, SIG_DEV_A));
        let paths = installed("handle_ok");
        let handle = UpdateHandle::with_parts(
            cfg(&server.base),
            TEST_KEY.to_owned(),
            paths.clone(),
            paths.exe.clone(),
            "old".into(),
            "dev-oldsha1".into(),
            Box::new(|spec| {
                let spec = spec.clone();
                std::thread::spawn(move || handoff::report_ready(&spec).unwrap());
                Ok(Box::new(Fake(None)))
            }),
            Arc::new(move || {
                q.fetch_add(1, Ordering::SeqCst);
            }),
            Duration::from_secs(5),
            Box::new(|_| {}),
        );
        assert!(matches!(handle.request("dev"), Ok(Outcome::Started { .. })));
        wait_until("handoff", || quits.load(Ordering::SeqCst) == 1);
        wait_until("last_update", || handle.last_json()["ok"] == true);
        assert_eq!(handle.last_json()["sha"], SHA_A);
        assert_eq!(fs::read(&paths.exe).unwrap(), PAYLOAD);
        assert_eq!(fs::read(&paths.old_exe).unwrap(), OLD_EXE);
        // The old instance is exiting: no second update may start.
        assert_eq!(handle.request("dev"), Err(UpdateError::Busy));
    }

    #[test]
    fn requests_are_checked_before_any_download_and_busy_is_panic_safe() {
        let rig = rig(
            "handle_checks",
            Box::new(|_| Err(io::Error::other("no spawn"))),
        );
        let hits = || rig._server.seen.lock().unwrap().len();
        assert_eq!(rig.handle.request("main"), Err(UpdateError::BadChannel));
        assert_eq!(rig.handle.request("../x"), Err(UpdateError::BadChannel));
        assert_eq!(hits(), 0);

        // Not running from the install path.
        let outside = UpdateHandle::with_parts(
            cfg("http://127.0.0.1:1/releases"),
            TEST_KEY.to_owned(),
            rig.paths.clone(),
            PathBuf::from("/elsewhere/tze_hud.exe"),
            "old".into(),
            "dev-old".into(),
            Box::new(|_| unreachable!()),
            Arc::new(|| {}),
            Duration::from_millis(50),
            Box::new(|_| {}),
        );
        assert_eq!(outside.request("dev"), Err(UpdateError::NotInstalled));
    }

    #[test]
    fn a_panic_on_the_update_thread_does_not_wedge_updates() {
        let server = serve(release_files("/releases/download/dev", PAYLOAD, SIG_DEV_A));
        let paths = installed("handle_panic");
        let handle = UpdateHandle::with_parts(
            cfg(&server.base),
            TEST_KEY.to_owned(),
            paths.clone(),
            paths.exe.clone(),
            "old".into(),
            "dev-old".into(),
            Box::new(|_| Err(io::Error::other("no spawn"))),
            Arc::new(|| {}),
            Duration::from_millis(50),
            // Runs on the update thread after the rollback.
            Box::new(|_| panic!("toast sink panics")),
        );
        assert!(matches!(handle.request("dev"), Ok(Outcome::Started { .. })));
        wait_until("busy cleared by unwinding", || {
            !handle.0.busy.load(Ordering::Acquire)
        });
        assert_eq!(fs::read(&paths.exe).unwrap(), OLD_EXE);
        assert!(matches!(handle.request("dev"), Ok(Outcome::Started { .. })));
    }

    #[test]
    fn the_embedded_public_key_is_a_valid_minisign_key() {
        PublicKey::decode(PUBLIC_KEY).expect("app/tze_hud_app/minisign.pub must parse");
    }
}
