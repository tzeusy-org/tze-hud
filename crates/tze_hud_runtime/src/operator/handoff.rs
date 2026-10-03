//! Health handoff: replace the running process with a new one and keep the old
//! one if the new one is not healthy (T6 step 5).
//!
//! Protocol. The old instance binds a one-shot loopback listener, starts the
//! new exe with `--handoff 127.0.0.1:<port>:<nonce>`, and waits up to
//! [`HANDOFF_TIMEOUT`] for one `READY {json}` line carrying the same nonce. The
//! new instance sends it once its first frame is submitted (window up, GPU
//! working), then takes over: it waits for the single-instance mutex, and only
//! after that binds the service ports (retrying for [`BIND_RETRY`]).
//!
//! * READY with the right nonce: the old instance shuts down (releasing its
//!   ports and the mutex) and exits 0.
//! * Wrong nonce, silence until the timeout, or the child exiting first: the
//!   child is killed and the old instance stays up, untouched.
//!
//! Orchestration is generic over how the child is started ([`ChildProc`]) and
//! every timeout is a parameter, so it is tested on any host with a fake child.
//! Nothing here accepts arguments from a request: the relaunch is the current
//! exe with its own validated argv (see [`child_args`]).

use std::io::{self, BufRead, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// How long the old instance waits for the new one to report healthy.
pub const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the new instance retries binding a service port once it has taken
/// over (the old instance is still releasing it).
pub const BIND_RETRY: Duration = Duration::from_secs(10);
/// Pause between bind attempts.
const BIND_RETRY_STEP: Duration = Duration::from_millis(100);
/// Poll step while waiting for the child.
const POLL_STEP: Duration = Duration::from_millis(5);
/// Longest READY line accepted.
const MAX_LINE: u64 = 4096;

/// Stops this instance cleanly (shutdown token plus an event-loop wake).
pub type Quit = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum HandoffError {
    #[error("could not open the handoff listener: {0}")]
    Listen(#[source] io::Error),
    #[error("could not start the new instance: {0}")]
    Spawn(#[source] io::Error),
    #[error("the new instance did not report ready within {0:?}")]
    Timeout(Duration),
    #[error("the new instance exited before reporting ready ({0})")]
    ChildExited(String),
    #[error("the new instance sent an invalid ready report: {0}")]
    BadReady(&'static str),
    #[error("could not check the new instance: {0}")]
    Wait(#[source] io::Error),
}

// ── Protocol ─────────────────────────────────────────────────────────────────

/// Where the new instance reports back, and the shared secret proving it is
/// the child that was started. Rendered as `127.0.0.1:<port>:<nonce>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    pub addr: SocketAddr,
    pub nonce: String,
}

impl ChildSpec {
    /// Parse `127.0.0.1:<port>:<nonce>`. Only loopback and a plain
    /// alphanumeric nonce are accepted.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut parts = text.splitn(3, ':');
        let (Some(ip), Some(port), Some(nonce)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(format!("expected <ip>:<port>:<nonce>, got {text:?}"));
        };
        let ip: Ipv4Addr = ip
            .parse()
            .map_err(|_| format!("handoff address {ip:?} is not an IPv4 address"))?;
        if !ip.is_loopback() {
            return Err(format!("handoff address {ip} is not loopback"));
        }
        let port: u16 = port
            .parse()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| format!("handoff port {port:?} is not a valid port"))?;
        if !(8..=64).contains(&nonce.len()) || !nonce.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err("handoff nonce must be 8-64 alphanumeric characters".into());
        }
        Ok(Self {
            addr: SocketAddr::from((ip, port)),
            nonce: nonce.to_owned(),
        })
    }

    /// The value that follows `--handoff`.
    pub fn arg_value(&self) -> String {
        format!("{}:{}", self.addr, self.nonce)
    }

    fn fresh(addr: SocketAddr) -> Self {
        Self {
            addr,
            nonce: uuid::Uuid::now_v7().simple().to_string(),
        }
    }
}

/// The new instance's ready report: `READY {json}\n`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    pub version: String,
    pub sha: String,
    pub pid: u32,
    pub nonce: String,
}

fn ready_line(ready: &Ready) -> String {
    format!(
        "READY {}\n",
        serde_json::to_string(ready).unwrap_or_default()
    )
}

fn parse_ready_line(line: &str, spec: &ChildSpec) -> Result<Ready, HandoffError> {
    let body = line
        .trim_end()
        .strip_prefix("READY ")
        .ok_or(HandoffError::BadReady("not a READY line"))?;
    let ready: Ready =
        serde_json::from_str(body).map_err(|_| HandoffError::BadReady("malformed READY json"))?;
    if ready.nonce != spec.nonce {
        return Err(HandoffError::BadReady("wrong nonce"));
    }
    Ok(ready)
}

/// The arguments for the relaunch: this process's own argv minus any previous
/// `--handoff [spec]`. The caller appends the new `--handoff <spec>`.
pub fn child_args(original: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(original.len());
    let mut it = original.iter().peekable();
    while let Some(arg) = it.next() {
        if arg == "--handoff" {
            // Drop the optional spec value too.
            if it.peek().is_some_and(|next| !next.starts_with("--")) {
                it.next();
            }
        } else {
            out.push(arg.clone());
        }
    }
    out
}

// ── Old side ─────────────────────────────────────────────────────────────────

/// The new instance, as the old side sees it.
pub trait ChildProc: Send {
    /// `Some(description)` once the child has exited.
    fn try_wait(&mut self) -> io::Result<Option<String>>;
    /// Stop the child and reap it. Best effort.
    fn kill(&mut self);
}

impl ChildProc for std::process::Child {
    fn try_wait(&mut self) -> io::Result<Option<String>> {
        std::process::Child::try_wait(self).map(|s| s.map(|s| s.to_string()))
    }

    fn kill(&mut self) {
        let _ = std::process::Child::kill(self);
        let _ = self.wait();
    }
}

/// Start `exe` with `args` plus `--handoff <spec>`, detached from this process
/// so it outlives it.
pub fn spawn_child(
    exe: &std::path::Path,
    args: &[String],
    spec: &ChildSpec,
) -> io::Result<Box<dyn ChildProc>> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(child_args(args))
        .arg("--handoff")
        .arg(spec.arg_value())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        cmd.creation_flags((CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS).0);
    }
    Ok(Box::new(cmd.spawn()?))
}

/// Start the new instance via `spawn` and wait up to `timeout` for it to
/// report ready. On success `on_ready` runs (the caller shuts this instance
/// down) and the child is left running. On any failure the child is killed and
/// nothing else happens: the caller keeps running.
pub fn handoff(
    spawn: impl FnOnce(&ChildSpec) -> io::Result<Box<dyn ChildProc>>,
    timeout: Duration,
    on_ready: impl FnOnce(),
) -> Result<Ready, HandoffError> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(HandoffError::Listen)?;
    listener
        .set_nonblocking(true)
        .map_err(HandoffError::Listen)?;
    let spec = ChildSpec::fresh(listener.local_addr().map_err(HandoffError::Listen)?);
    let deadline = Instant::now() + timeout;
    let mut child = spawn(&spec).map_err(HandoffError::Spawn)?;
    match wait_ready(&listener, &spec, child.as_mut(), deadline, timeout) {
        Ok(ready) => {
            on_ready();
            Ok(ready)
        }
        Err(e) => {
            child.kill();
            Err(e)
        }
    }
}

fn wait_ready(
    listener: &TcpListener,
    spec: &ChildSpec,
    child: &mut dyn ChildProc,
    deadline: Instant,
    timeout: Duration,
) -> Result<Ready, HandoffError> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => return read_ready(stream, spec, deadline, timeout),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(HandoffError::Listen(e)),
        }
        if let Some(status) = child.try_wait().map_err(HandoffError::Wait)? {
            return Err(HandoffError::ChildExited(status));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(HandoffError::Timeout(timeout));
        }
        std::thread::sleep(POLL_STEP.min(deadline - now));
    }
}

fn read_ready(
    stream: TcpStream,
    spec: &ChildSpec,
    deadline: Instant,
    timeout: Duration,
) -> Result<Ready, HandoffError> {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    stream.set_nonblocking(false).map_err(HandoffError::Wait)?;
    stream
        .set_read_timeout(Some(remaining))
        .map_err(HandoffError::Wait)?;
    let mut line = String::new();
    match io::BufReader::new(stream.take(MAX_LINE)).read_line(&mut line) {
        Ok(0) => Err(HandoffError::BadReady("connection closed without a line")),
        Ok(_) => parse_ready_line(&line, spec),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Err(HandoffError::Timeout(timeout))
        }
        Err(e) => Err(HandoffError::Wait(e)),
    }
}

// ── Restart (POST /admin/restart) ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartError {
    /// A restart is already in progress.
    Busy,
    /// The restart thread could not be started.
    Unavailable,
}

type Spawner = dyn Fn(&ChildSpec) -> io::Result<Box<dyn ChildProc>> + Send + Sync;

struct RestartInner {
    spawn: Box<Spawner>,
    quit: Quit,
    timeout: Duration,
    busy: AtomicBool,
    last: Mutex<Option<Result<Ready, String>>>,
}

/// Relaunches this exe with its own argv through [`handoff`]. One restart at a
/// time; cheap to clone.
#[derive(Clone)]
pub struct RestartHandle(Arc<RestartInner>);

impl std::fmt::Debug for RestartHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestartHandle").finish_non_exhaustive()
    }
}

impl RestartHandle {
    /// Restart `exe` with `original_args` (this process's own argv).
    pub fn new(exe: PathBuf, original_args: Vec<String>, quit: Quit) -> Self {
        Self::with_spawner(
            Box::new(move |spec| spawn_child(&exe, &original_args, spec)),
            quit,
            HANDOFF_TIMEOUT,
        )
    }

    pub fn with_spawner(spawn: Box<Spawner>, quit: Quit, timeout: Duration) -> Self {
        Self(Arc::new(RestartInner {
            spawn,
            quit,
            timeout,
            busy: AtomicBool::new(false),
            last: Mutex::new(None),
        }))
    }

    /// Start a restart in the background and return at once. The caller
    /// answers 202; the outcome shows up in [`Self::last_json`].
    pub fn request(&self) -> Result<(), RestartError> {
        if self.0.busy.swap(true, Ordering::AcqRel) {
            return Err(RestartError::Busy);
        }
        let inner = Arc::clone(&self.0);
        let started = std::thread::Builder::new()
            .name("restart".into())
            .spawn(move || {
                let quit = Arc::clone(&inner.quit);
                let result = handoff(|spec| (inner.spawn)(spec), inner.timeout, move || quit());
                match &result {
                    // Stay busy: this instance is shutting down.
                    Ok(ready) => tracing::info!(
                        pid = ready.pid,
                        "restart: new instance is ready; shutting down"
                    ),
                    Err(e) => {
                        tracing::error!(error = %e, "restart failed; staying up");
                        inner.busy.store(false, Ordering::Release);
                    }
                }
                *inner.last.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(result.map_err(|e| e.to_string()));
            });
        if started.is_err() {
            self.0.busy.store(false, Ordering::Release);
            return Err(RestartError::Unavailable);
        }
        Ok(())
    }

    /// `last_restart` for `/admin/status`: `null` until a restart finishes.
    pub fn last_json(&self) -> Value {
        match &*self.0.last.lock().unwrap_or_else(|e| e.into_inner()) {
            None => Value::Null,
            Some(Ok(r)) => json!({"ok": true, "pid": r.pid, "error": Value::Null}),
            Some(Err(why)) => json!({"ok": false, "pid": Value::Null, "error": why}),
        }
    }
}

// ── New side ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateState {
    Closed,
    Open,
    Failed,
}

/// Holds back the new instance's service ports until it has taken over from
/// the old one. Cheap to clone.
#[derive(Clone)]
pub struct BindGate {
    tx: Arc<tokio::sync::watch::Sender<GateState>>,
    quit: Arc<OnceLock<Quit>>,
}

impl std::fmt::Debug for BindGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BindGate").finish_non_exhaustive()
    }
}

impl BindGate {
    pub fn closed() -> Self {
        Self {
            tx: Arc::new(tokio::sync::watch::channel(GateState::Closed).0),
            quit: Arc::new(OnceLock::new()),
        }
    }

    /// Where a failed takeover stops this instance. Set once by the runtime.
    pub fn install_quit(&self, quit: Quit) {
        let _ = self.quit.set(quit);
    }

    /// The old instance is gone; listeners may bind.
    pub fn open(&self) {
        self.tx.send_replace(GateState::Open);
    }

    /// The takeover failed (or a port never freed): stop this instance.
    pub fn fail(&self, why: &str) {
        tracing::error!(%why, "handoff takeover failed; shutting down");
        self.tx.send_replace(GateState::Failed);
        if let Some(quit) = self.quit.get() {
            quit();
        }
    }

    /// Wait for the gate: `true` once open, `false` if the takeover failed.
    pub async fn wait(&self) -> bool {
        let mut rx = self.tx.subscribe();
        loop {
            match *rx.borrow_and_update() {
                GateState::Open => return true,
                GateState::Failed => return false,
                GateState::Closed => {}
            }
            if rx.changed().await.is_err() {
                return false;
            }
        }
    }
}

/// Call `attempt` until it succeeds or `window` has passed (the last error is
/// returned).
pub async fn retry_bind<T>(
    window: Duration,
    mut attempt: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let deadline = Instant::now() + window;
    loop {
        match attempt() {
            Ok(v) => return Ok(v),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => tokio::time::sleep(BIND_RETRY_STEP).await,
        }
    }
}

/// Everything the new instance needs: where to report, and the gate.
#[derive(Clone)]
pub struct HandoffChild {
    spec: ChildSpec,
    gate: BindGate,
    fired: Arc<AtomicBool>,
}

impl std::fmt::Debug for HandoffChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandoffChild")
            .field("addr", &self.spec.addr)
            .finish_non_exhaustive()
    }
}

impl HandoffChild {
    pub fn new(spec: ChildSpec) -> Self {
        Self {
            spec,
            gate: BindGate::closed(),
            fired: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn gate(&self) -> &BindGate {
        &self.gate
    }

    /// Call when the first frame has been submitted. Once only (later calls
    /// are a cheap no-op). Never blocks the caller: reports READY, waits for
    /// the single-instance mutex, then opens the gate.
    pub fn first_present(&self) {
        if self.fired.swap(true, Ordering::AcqRel) {
            return;
        }
        let (spec, gate) = (self.spec.clone(), self.gate.clone());
        let spawned = std::thread::Builder::new()
            .name("handoff-ready".into())
            .spawn(move || {
                if let Err(e) = report_ready(&spec) {
                    return gate.fail(&format!("could not report ready: {e}"));
                }
                match super::install::acquire_single_instance(super::install::HANDOFF_WAIT) {
                    super::install::Acquire::Acquired(guard) => {
                        tracing::info!("handoff: took over the single-instance mutex");
                        gate.open();
                        // Hold the mutex (this thread parks) until the process exits.
                        loop {
                            std::thread::park();
                            let _held = &guard;
                        }
                    }
                    super::install::Acquire::AlreadyRunning => {
                        gate.fail(
                            "the previous instance did not release the single-instance mutex",
                        );
                    }
                }
            });
        if let Err(e) = spawned {
            self.gate
                .fail(&format!("could not start the handoff thread: {e}"));
        }
    }
}

/// Send the READY line to the old instance.
pub fn report_ready(spec: &ChildSpec) -> io::Result<()> {
    use std::io::Write;
    let ready = Ready {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        sha: super::status::build_sha(),
        pid: std::process::id(),
        nonce: spec.nonce.clone(),
    };
    let mut stream = TcpStream::connect_timeout(&spec.addr, Duration::from_secs(5))?;
    stream.write_all(ready_line(&ready).as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const SHORT: Duration = Duration::from_millis(150);

    /// A stand-in for the new instance: runs `script` on a thread when
    /// started, and reports whether it was killed.
    struct Fake {
        exited: Arc<Mutex<Option<String>>>,
        killed: Arc<AtomicBool>,
    }

    impl ChildProc for Fake {
        fn try_wait(&mut self) -> io::Result<Option<String>> {
            Ok(self.exited.lock().unwrap().clone())
        }
        fn kill(&mut self) {
            self.killed.store(true, Ordering::SeqCst);
        }
    }

    enum Behaviour {
        Ready,
        WrongNonce,
        Silent,
        ExitEarly,
    }

    fn run(behaviour: Behaviour, timeout: Duration) -> (Result<Ready, HandoffError>, bool, usize) {
        let killed = Arc::new(AtomicBool::new(false));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let (k, s) = (Arc::clone(&killed), Arc::clone(&shutdowns));
        let result = handoff(
            move |spec| {
                let exited = Arc::new(Mutex::new(None));
                match behaviour {
                    Behaviour::Ready | Behaviour::WrongNonce => {
                        let mut spec = spec.clone();
                        if matches!(behaviour, Behaviour::WrongNonce) {
                            spec.nonce = "wrongnonce".into();
                        }
                        std::thread::spawn(move || report_ready(&spec).unwrap());
                    }
                    Behaviour::Silent => {}
                    Behaviour::ExitEarly => *exited.lock().unwrap() = Some("exit status: 3".into()),
                }
                Ok(Box::new(Fake { exited, killed: k }))
            },
            timeout,
            move || {
                s.fetch_add(1, Ordering::SeqCst);
            },
        );
        (
            result,
            killed.load(Ordering::SeqCst),
            shutdowns.load(Ordering::SeqCst),
        )
    }

    #[test]
    fn ready_with_the_right_nonce_shuts_the_old_instance_down_and_keeps_the_child() {
        let (result, killed, shutdowns) = run(Behaviour::Ready, Duration::from_secs(5));
        let ready = result.unwrap();
        assert_eq!(ready.pid, std::process::id());
        assert!(!killed);
        assert_eq!(shutdowns, 1);
    }

    #[test]
    fn wrong_nonce_silence_and_early_exit_kill_the_child_and_keep_the_old_instance() {
        let (r, killed, shutdowns) = run(Behaviour::WrongNonce, Duration::from_secs(5));
        assert!(
            matches!(r, Err(HandoffError::BadReady("wrong nonce"))),
            "{r:?}"
        );
        assert!(killed && shutdowns == 0);

        let (r, killed, shutdowns) = run(Behaviour::Silent, SHORT);
        assert!(matches!(r, Err(HandoffError::Timeout(_))), "{r:?}");
        assert!(killed && shutdowns == 0);

        let (r, killed, shutdowns) = run(Behaviour::ExitEarly, Duration::from_secs(5));
        assert!(matches!(r, Err(HandoffError::ChildExited(_))), "{r:?}");
        assert!(killed && shutdowns == 0);
    }

    #[test]
    fn spawn_failure_is_an_error_and_nothing_shuts_down() {
        let shut = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&shut);
        let r = handoff(
            |_| Err(io::Error::other("no such exe")),
            SHORT,
            move || s.store(true, Ordering::SeqCst),
        );
        assert!(matches!(r, Err(HandoffError::Spawn(_))));
        assert!(!shut.load(Ordering::SeqCst));
    }

    #[test]
    fn spec_round_trips_and_rejects_non_loopback_or_odd_nonces() {
        let spec = ChildSpec::parse("127.0.0.1:4242:abcdef0123456789").unwrap();
        assert_eq!(ChildSpec::parse(&spec.arg_value()).unwrap(), spec);
        for bad in [
            "10.0.0.1:4242:abcdef0123456789",
            "127.0.0.1:0:abcdef0123456789",
            "127.0.0.1:4242:short",
            "127.0.0.1:4242:abc def0123456789",
            "127.0.0.1:4242",
            "localhost:4242:abcdef0123456789",
        ] {
            assert!(ChildSpec::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn child_args_drop_a_previous_handoff_and_add_nothing_else() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let base = a(&["--config", "c.toml", "--window-mode", "overlay"]);
        assert_eq!(child_args(&base), base);
        let mut bare = base.clone();
        bare.push("--handoff".into());
        assert_eq!(child_args(&bare), base);
        let mut spec = a(&["--handoff", "127.0.0.1:1:abcdef0123"]);
        spec.extend(base.clone());
        assert_eq!(child_args(&spec), base);
        // A bare --handoff before another flag keeps that flag.
        assert_eq!(
            child_args(&a(&["--handoff", "--mcp-port", "9"])),
            a(&["--mcp-port", "9"])
        );
    }

    #[test]
    fn restart_is_single_flight_and_failure_leaves_it_retryable() {
        let quit_calls = Arc::new(AtomicUsize::new(0));
        let q = Arc::clone(&quit_calls);
        let h = RestartHandle::with_spawner(
            Box::new(|_| {
                Ok(Box::new(Fake {
                    exited: Arc::new(Mutex::new(None)),
                    killed: Arc::new(AtomicBool::new(false)),
                }))
            }),
            Arc::new(move || {
                q.fetch_add(1, Ordering::SeqCst);
            }),
            SHORT,
        );
        assert_eq!(h.last_json(), Value::Null);
        h.request().unwrap();
        assert_eq!(h.request(), Err(RestartError::Busy));
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.last_json().is_null() {
            assert!(Instant::now() < deadline, "restart never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(h.last_json()["ok"], json!(false));
        assert_eq!(quit_calls.load(Ordering::SeqCst), 0);
        // The silent child timed out, so the old instance may try again.
        h.request().unwrap();
    }

    #[tokio::test]
    async fn gate_opens_fails_and_retry_bind_gives_up() {
        let gate = BindGate::closed();
        let quits = Arc::new(AtomicUsize::new(0));
        let q = Arc::clone(&quits);
        gate.install_quit(Arc::new(move || {
            q.fetch_add(1, Ordering::SeqCst);
        }));
        let waiter = tokio::spawn({
            let g = gate.clone();
            async move { g.wait().await }
        });
        tokio::task::yield_now().await;
        gate.open();
        assert!(waiter.await.unwrap());

        let failed = BindGate::closed();
        let fq = Arc::clone(&quits);
        failed.install_quit(Arc::new(move || {
            fq.fetch_add(10, Ordering::SeqCst);
        }));
        failed.fail("test");
        assert!(!failed.wait().await);
        assert_eq!(
            quits.load(Ordering::SeqCst),
            10,
            "only the failed gate quits"
        );

        let mut tries = 0;
        let ok = retry_bind(Duration::from_secs(2), || {
            tries += 1;
            if tries < 3 {
                Err(io::Error::other("busy"))
            } else {
                Ok(tries)
            }
        })
        .await;
        assert_eq!(ok.unwrap(), 3);
        let err = retry_bind(Duration::from_millis(150), || -> io::Result<()> {
            Err(io::Error::other("busy"))
        })
        .await;
        assert!(err.is_err());
    }
}
