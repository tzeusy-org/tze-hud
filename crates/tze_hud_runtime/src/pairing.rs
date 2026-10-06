//! First-run pairing (`POST /pair`).
//!
//! While pairing is open the HUD shows a 6-digit one-time code and its address
//! on the system card. The owner gives the code to an agent, which trades it
//! for a freshly generated PSK:
//!
//! ```text
//! curl -s host:9090/pair -d '{"agent":"claude","code":"482913"}'
//! -> {"agent":"claude","psk":"<64 hex>","mcp":"http://100.x.y.z:9090/mcp","grpc":"100.x.y.z:50051"}
//! ```
//!
//! Pairing is open at startup while no agent exists, and again after the owner
//! asks for it (`tze_hud --pair`, Ctrl+Shift+P). A code is single use and
//! valid for [`CODE_TTL`]. After [`MAX_BAD_ATTEMPTS`] wrong guesses it is
//! replaced by a new one; the [`MAX_REGENERATIONS`]th such replacement closes
//! pairing for [`COOLDOWN`] instead. Only the SHA-256 of the PSK is stored
//! (`agents.toml`); neither the code nor the PSK is ever logged.
//!
//! [`PairingState`] is the pure decision core: every method that depends on
//! the clock takes `now` (docs/invariants.md section 9). [`Pairing`] wires it
//! to the agent store, the system card, and the HTTP route.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use subtle::ConstantTimeEq;
use tze_hud_config::agents_file::{self, AgentRecord};
use tze_hud_scene::config::SharedAgents;

use crate::http::{OperatorCode, OperatorError, Response};
use crate::shell::system_card::{SystemCard, SystemCardHandle};

/// How long a code stays valid.
pub const CODE_TTL: Duration = Duration::from_secs(5 * 60);
/// Wrong guesses a code survives; the next one replaces it.
pub const MAX_BAD_ATTEMPTS: u32 = 5;
/// Forced replacements per opening before pairing cools down: the last one
/// closes pairing instead of issuing a code.
pub const MAX_REGENERATIONS: u32 = 3;
/// How long pairing stays closed after too many failed codes.
pub const COOLDOWN: Duration = Duration::from_secs(60);

const CODE_DIGITS: usize = 6;
const CODE_SPACE: u32 = 1_000_000;

/// Draw a code from `next_word` by rejection sampling: words from the biased
/// tail of the `u32` range are discarded so every code is equally likely.
fn sample_code(mut next_word: impl FnMut() -> u32) -> [u8; CODE_DIGITS] {
    let limit = u32::MAX - u32::MAX % CODE_SPACE;
    let n = loop {
        let word = next_word();
        if word < limit {
            break word % CODE_SPACE;
        }
    };
    let mut digits = [b'0'; CODE_DIGITS];
    let mut rest = n;
    for d in digits.iter_mut().rev() {
        *d = b'0' + (rest % 10) as u8;
        rest /= 10;
    }
    digits
}

fn os_word() -> u32 {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("OS random source");
    u32::from_le_bytes(bytes)
}

/// A code shown to the owner. Deliberately not `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuedCode {
    pub code: String,
    pub expires_at: Instant,
}

/// Result of opening pairing.
#[derive(Clone, PartialEq, Eq)]
pub enum Opened {
    Code(IssuedCode),
    CoolingDown,
}

/// Result of one submitted code.
#[derive(Clone, PartialEq, Eq)]
pub enum Attempt {
    /// Right code: it is spent and pairing is closed.
    Accepted,
    /// Wrong code. `next` says what the failure did to the open code.
    Wrong { next: AfterWrong },
    /// Pairing is not open (never opened, spent, expired, or cooling down).
    Closed,
}

/// What a wrong guess did to the code.
#[derive(Clone, PartialEq, Eq)]
pub enum AfterWrong {
    /// Same code, fewer attempts left.
    Unchanged,
    /// Too many guesses: the old code is dead and this one replaces it.
    Regenerated(IssuedCode),
    /// Too many regenerations: pairing is closed for [`COOLDOWN`].
    CoolingDown,
}

struct OpenCode {
    digits: [u8; CODE_DIGITS],
    expires_at: Instant,
    bad_attempts: u32,
}

/// The pairing decisions, with no I/O and no clock of its own.
pub struct PairingState {
    next_word: Box<dyn FnMut() -> u32 + Send>,
    open: Option<OpenCode>,
    regenerations: u32,
    cooldown_until: Option<Instant>,
}

impl PairingState {
    pub fn new(next_word: impl FnMut() -> u32 + Send + 'static) -> Self {
        Self {
            next_word: Box::new(next_word),
            open: None,
            regenerations: 0,
            cooldown_until: None,
        }
    }

    /// Pairing state fed by the OS random source.
    pub fn system() -> Self {
        Self::new(os_word)
    }

    fn cooling_down(&self, now: Instant) -> bool {
        self.cooldown_until.is_some_and(|until| now < until)
    }

    fn issue(&mut self, now: Instant) -> IssuedCode {
        let digits = sample_code(&mut self.next_word);
        let expires_at = now + CODE_TTL;
        self.open = Some(OpenCode {
            digits,
            expires_at,
            bad_attempts: 0,
        });
        IssuedCode {
            code: String::from_utf8_lossy(&digits).into_owned(),
            expires_at,
        }
    }

    /// Open pairing with a fresh code, replacing any open one, unless cooling
    /// down.
    pub fn open_at(&mut self, now: Instant) -> Opened {
        if self.cooling_down(now) {
            return Opened::CoolingDown;
        }
        self.cooldown_until = None;
        self.regenerations = 0;
        Opened::Code(self.issue(now))
    }

    /// Whether a code can currently be redeemed.
    pub fn is_open_at(&self, now: Instant) -> bool {
        !self.cooling_down(now) && self.open.as_ref().is_some_and(|o| now <= o.expires_at)
    }

    /// Check a submitted code. The comparison is constant time.
    pub fn attempt_at(&mut self, now: Instant, submitted: &str) -> Attempt {
        if !self.is_open_at(now) {
            self.open = None;
            return Attempt::Closed;
        }
        let open = self.open.as_mut().expect("is_open_at checked");
        let matches = submitted.len() == CODE_DIGITS
            && bool::from(open.digits.as_slice().ct_eq(submitted.as_bytes()));
        if matches {
            self.open = None;
            self.regenerations = 0;
            return Attempt::Accepted;
        }
        open.bad_attempts += 1;
        if open.bad_attempts < MAX_BAD_ATTEMPTS {
            return Attempt::Wrong {
                next: AfterWrong::Unchanged,
            };
        }
        self.regenerations += 1;
        if self.regenerations >= MAX_REGENERATIONS {
            self.open = None;
            self.cooldown_until = Some(now + COOLDOWN);
            return Attempt::Wrong {
                next: AfterWrong::CoolingDown,
            };
        }
        Attempt::Wrong {
            next: AfterWrong::Regenerated(self.issue(now)),
        }
    }

    /// Re-open after a failed redemption (the spent code could not be used).
    fn reopen_after_failure(&mut self, now: Instant) -> IssuedCode {
        self.issue(now)
    }
}

/// Time source for pairing: monotonic for expiry, wall for the card.
pub trait PairClock: Send + Sync {
    fn now(&self) -> Instant;
    fn wall_us(&self) -> u64;
}

struct SystemPairClock;

impl PairClock for SystemPairClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wall_us(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros() as u64)
    }
}

/// `POST /pair` request body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairRequest {
    agent: String,
    code: String,
    #[serde(default)]
    admin: bool,
}

/// Agent ids are `[a-z0-9-]{1,32}`.
fn valid_agent_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn new_psk() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random source");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// RFC 3339 UTC timestamp (second precision) for `paired_at`.
fn rfc3339_utc(wall_us: u64) -> String {
    let secs = wall_us / 1_000_000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil-from-days (Howard Hinnant), valid for the proleptic Gregorian calendar.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// The pairing service behind `POST /pair`, the system card, and the owner's
/// triggers (`--pair`, Ctrl+Shift+P).
pub struct Pairing {
    state: Mutex<PairingState>,
    agents: SharedAgents,
    agents_path: Option<PathBuf>,
    card: SystemCardHandle,
    grpc_port: u16,
    clock: Arc<dyn PairClock>,
    /// Addresses the HTTP port is bound on; shared with `/admin/status`.
    binds: Arc<Mutex<Vec<SocketAddr>>>,
    /// Windows Firewall check for the tailnet address, shared with `/admin/status`.
    firewall: Arc<crate::firewall::FirewallProbe>,
}

/// The pairing card's lines: code, address, validity, and (only when Windows
/// Firewall blocks the tailnet address) one warning.
fn pairing_lines(
    code: &str,
    address: String,
    firewall: Option<&crate::firewall::TailnetInbound>,
) -> Vec<String> {
    let mut lines = vec![
        code.to_owned(),
        address,
        format!("valid for {} minutes", CODE_TTL.as_secs() / 60),
    ];
    if firewall.is_some_and(crate::firewall::TailnetInbound::is_blocked) {
        lines.push(
            "Windows Firewall blocks remote agents: see windows-install.md#remote-agents"
                .to_owned(),
        );
    }
    lines
}

impl std::fmt::Debug for Pairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pairing").finish_non_exhaustive()
    }
}

impl Pairing {
    pub(crate) fn new(
        state: PairingState,
        clock: Arc<dyn PairClock>,
        agents: SharedAgents,
        agents_path: Option<PathBuf>,
        card: SystemCardHandle,
        grpc_port: u16,
    ) -> Self {
        let binds: Arc<Mutex<Vec<SocketAddr>>> = Arc::default();
        Self {
            state: Mutex::new(state),
            agents,
            agents_path,
            card,
            grpc_port,
            clock,
            firewall: Arc::new(crate::firewall::FirewallProbe::system(
                Arc::clone(&binds),
                vec![grpc_port],
            )),
            binds,
        }
    }

    pub(crate) fn firewall(&self) -> Arc<crate::firewall::FirewallProbe> {
        Arc::clone(&self.firewall)
    }

    pub(crate) fn system(
        agents: SharedAgents,
        agents_path: Option<PathBuf>,
        card: SystemCardHandle,
        grpc_port: u16,
    ) -> Self {
        Self::new(
            PairingState::system(),
            Arc::new(SystemPairClock),
            agents,
            agents_path,
            card,
            grpc_port,
        )
    }

    /// The listener addresses, seeded with `initial` the first time.
    pub(crate) fn binds(&self, initial: &[SocketAddr]) -> Arc<Mutex<Vec<SocketAddr>>> {
        let mut binds = self.binds.lock().unwrap_or_else(|e| e.into_inner());
        if binds.is_empty() {
            binds.extend_from_slice(initial);
        }
        Arc::clone(&self.binds)
    }

    /// The address to hand out: the Tailscale one if bound, else the first.
    fn endpoint(binds: &[SocketAddr]) -> Option<SocketAddr> {
        binds
            .iter()
            .find(|a| !crate::net_addrs::tailnet_addrs(&[a.ip()]).is_empty())
            .or(binds.first())
            .copied()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PairingState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn show(&self, issued: &IssuedCode) {
        let (endpoint, address) = {
            let binds = self.binds.lock().unwrap_or_else(|e| e.into_inner());
            let endpoint = Self::endpoint(&binds);
            let address = endpoint.map_or_else(
                || "this machine, port 9090".to_owned(),
                |a| format!("http://{a}/pair"),
            );
            (endpoint, address)
        };
        let on_tailnet =
            endpoint.is_some_and(|a| !crate::net_addrs::tailnet_addrs(&[a.ip()]).is_empty());
        // Never wait on the firewall here: use a fresh cached answer, else show
        // the card now and add the warning when the background check finishes.
        let known = on_tailnet.then(|| self.firewall.cached()).flatten();
        let card = SystemCard {
            kind: tze_hud_compositor::SystemCardKind::Pairing,
            title: "Pair an agent".to_owned(),
            lines: pairing_lines(&issued.code, address.clone(), known.as_ref()),
            // Wall-clock deadline, taken at the same moment as the monotonic
            // code expiry; they can drift apart only if the system clock is
            // adjusted within the 5 minutes. The text is a fixed validity, not
            // a countdown (a countdown would redraw every second).
            expires_at_wall_us: Some(self.clock.wall_us() + CODE_TTL.as_micros() as u64),
        };
        self.card.set(card.clone());
        if on_tailnet && known.is_none() {
            let (probe, handle, code) = (
                Arc::clone(&self.firewall),
                self.card.clone(),
                issued.code.clone(),
            );
            std::thread::spawn(move || {
                let verdict = probe.current();
                if verdict.is_blocked() {
                    let mut with_warning = card.clone();
                    with_warning.lines = pairing_lines(&code, address, Some(&verdict));
                    handle.replace_if_current(&card, with_warning);
                }
            });
        }
    }

    fn paused_toast(&self) {
        self.card.set(SystemCard::toast(
            "Pairing paused; try again in a minute",
            self.clock.wall_us(),
        ));
    }

    /// Open pairing and show the code. Called at startup with no agents, and
    /// when the owner asks (`--pair`, Ctrl+Shift+P).
    pub fn open(&self) {
        match self.lock().open_at(self.clock.now()) {
            Opened::Code(issued) => {
                tracing::info!("pairing open; code shown on the HUD");
                self.show(&issued);
            }
            Opened::CoolingDown => {
                tracing::warn!("pairing is cooling down after too many wrong codes");
                self.paused_toast();
            }
        }
    }

    /// Serve `POST /pair`.
    pub fn pair(&self, body: &[u8]) -> Response {
        let bad_request = |hint: &str| {
            Response::operator_error(400, &OperatorError::new(OperatorCode::BadRequest, hint))
        };
        let Ok(req) = serde_json::from_slice::<PairRequest>(body) else {
            return bad_request(r#"send {"agent":"<id>","code":"<6 digits>"} as JSON"#);
        };
        // Checked before the code so a bad id cannot reveal whether the code
        // was right.
        if !valid_agent_id(&req.agent) {
            return bad_request("agent must be 1-32 characters of a-z, 0-9 and -");
        }
        let mut state = self.lock();
        let now = self.clock.now();
        match state.attempt_at(now, &req.code) {
            Attempt::Closed => Response::operator_error(
                403,
                &OperatorError::new(
                    OperatorCode::PairingClosed,
                    "press Ctrl+Shift+P on the HUD or run tze_hud --pair, then retry with the new code",
                ),
            ),
            Attempt::Wrong { next } => {
                match next {
                    AfterWrong::Unchanged => {}
                    AfterWrong::Regenerated(issued) => {
                        tracing::warn!("pairing code replaced after too many wrong attempts");
                        self.show(&issued);
                    }
                    AfterWrong::CoolingDown => {
                        tracing::warn!("pairing closed for a cooldown after too many wrong codes");
                        self.card.clear();
                    }
                }
                Response::operator_error(
                    403,
                    &OperatorError::new(
                        OperatorCode::PairCodeInvalid,
                        "wrong code; read the current one off the HUD",
                    ),
                )
            }
            Attempt::Accepted => match self.redeem(&req) {
                Ok(psk) => {
                    drop(state);
                    self.card.clear();
                    self.card.set(SystemCard::toast(
                        format!("Paired {}", req.agent),
                        self.clock.wall_us(),
                    ));
                    tracing::info!(agent = %req.agent, admin = req.admin, "agent paired");
                    self.success(&req.agent, &psk)
                }
                Err(why) => {
                    tracing::error!(error = %why, "pairing could not save the agent");
                    let issued = state.reopen_after_failure(now);
                    self.show(&issued);
                    Response::operator_error(
                        503,
                        &OperatorError::new(
                            OperatorCode::Unavailable,
                            "could not save agents.toml; a new code is on the HUD",
                        ),
                    )
                }
            },
        }
    }

    /// Persist the new agent (hash only), then swap the live directory.
    fn redeem(&self, req: &PairRequest) -> Result<String, String> {
        let path = self.agents_path.as_ref().ok_or("no agents.toml location")?;
        let mut file = agents_file::load(path).map_err(|e| e.to_string())?;
        let psk = new_psk();
        let mut allow = vec!["*".to_owned()];
        if req.admin {
            allow.push("admin".to_owned());
        }
        file.agents.insert(
            req.agent.clone(),
            AgentRecord {
                psk_sha256: agents_file::hash_psk_hex(&psk),
                allow,
                paired_at: Some(rfc3339_utc(self.clock.wall_us())),
            },
        );
        let directory = file.directory().map_err(|e| e.to_string())?;
        agents_file::save_atomic(path, &file).map_err(|e| e.to_string())?;
        self.agents.store(Arc::new(directory));
        Ok(psk)
    }

    fn success(&self, agent: &str, psk: &str) -> Response {
        let binds = self.binds.lock().unwrap_or_else(|e| e.into_inner());
        let host =
            Self::endpoint(&binds).unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 9090)));
        let grpc = SocketAddr::new(host.ip(), self.grpc_port);
        Response::json(
            serde_json::json!({
                "agent": agent,
                "psk": psk,
                "mcp": format!("http://{host}/mcp"),
                "grpc": grpc.to_string(),
            })
            .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tze_hud_compositor::SystemCardKind;

    /// A clock the test advances by hand.
    struct TestClock {
        base: Instant,
        offset_ms: AtomicU64,
    }

    impl TestClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                base: Instant::now(),
                offset_ms: AtomicU64::new(0),
            })
        }

        fn advance(&self, by: Duration) {
            self.offset_ms
                .fetch_add(by.as_millis() as u64, Ordering::SeqCst);
        }

        fn at(&self) -> Instant {
            self.now()
        }
    }

    impl PairClock for TestClock {
        fn now(&self) -> Instant {
            self.base + Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
        }

        fn wall_us(&self) -> u64 {
            1_000_000 + self.offset_ms.load(Ordering::SeqCst) * 1_000
        }
    }

    /// Codes 111111, 222222, ... one per draw.
    fn counting_words() -> impl FnMut() -> u32 + Send {
        let mut n = 0;
        move || {
            n += 1;
            n * 111_111
        }
    }

    fn code_of(opened: Opened) -> String {
        match opened {
            Opened::Code(issued) => issued.code,
            Opened::CoolingDown => panic!("expected a code"),
        }
    }

    #[test]
    fn rejection_sampling_discards_the_biased_tail() {
        let limit = u32::MAX - u32::MAX % CODE_SPACE;
        let mut words = [u32::MAX, limit, limit - 1].into_iter();
        // The first two words fall in the biased tail and are skipped.
        assert_eq!(&sample_code(|| words.next().unwrap()), b"999999");
        assert_eq!(&sample_code(|| 42), b"000042");
    }

    #[test]
    fn code_is_single_use() {
        let clock = TestClock::new();
        let mut state = PairingState::new(counting_words());
        let code = code_of(state.open_at(clock.at()));
        assert!(matches!(
            state.attempt_at(clock.at(), &code),
            Attempt::Accepted
        ));
        assert!(matches!(
            state.attempt_at(clock.at(), &code),
            Attempt::Closed
        ));
    }

    #[test]
    fn code_expires_after_five_minutes() {
        let clock = TestClock::new();
        let mut state = PairingState::new(counting_words());
        let code = code_of(state.open_at(clock.at()));
        clock.advance(CODE_TTL);
        assert!(state.is_open_at(clock.at()), "still valid at exactly 5 min");
        clock.advance(Duration::from_millis(1));
        assert!(matches!(
            state.attempt_at(clock.at(), &code),
            Attempt::Closed
        ));
    }

    #[test]
    fn five_wrong_codes_replace_the_code_and_three_replacements_cool_down() {
        let clock = TestClock::new();
        let mut state = PairingState::new(counting_words());
        let first = code_of(state.open_at(clock.at()));
        let wrong = "000000";
        for _ in 0..MAX_BAD_ATTEMPTS - 1 {
            assert!(matches!(
                state.attempt_at(clock.at(), wrong),
                Attempt::Wrong {
                    next: AfterWrong::Unchanged
                }
            ));
        }
        let Attempt::Wrong {
            next: AfterWrong::Regenerated(second),
        } = state.attempt_at(clock.at(), wrong)
        else {
            panic!("fifth wrong code must regenerate");
        };
        assert_ne!(second.code, first);
        // The old code is dead even though it is correct.
        assert!(matches!(
            state.attempt_at(clock.at(), &first),
            Attempt::Wrong { .. }
        ));
        // The probe above was round two's first wrong code; round two ends in
        // a second replacement and round three in the cooldown.
        for _ in 0..MAX_BAD_ATTEMPTS - 2 {
            assert!(matches!(
                state.attempt_at(clock.at(), wrong),
                Attempt::Wrong {
                    next: AfterWrong::Unchanged
                }
            ));
        }
        assert!(matches!(
            state.attempt_at(clock.at(), wrong),
            Attempt::Wrong {
                next: AfterWrong::Regenerated(_)
            }
        ));
        for _ in 0..MAX_BAD_ATTEMPTS - 1 {
            state.attempt_at(clock.at(), wrong);
        }
        assert!(matches!(
            state.attempt_at(clock.at(), wrong),
            Attempt::Wrong {
                next: AfterWrong::CoolingDown
            }
        ));
        assert!(matches!(
            state.attempt_at(clock.at(), wrong),
            Attempt::Closed
        ));
        assert!(matches!(state.open_at(clock.at()), Opened::CoolingDown));
        clock.advance(COOLDOWN - Duration::from_millis(1));
        assert!(matches!(state.open_at(clock.at()), Opened::CoolingDown));
        clock.advance(Duration::from_millis(1));
        assert!(matches!(state.open_at(clock.at()), Opened::Code(_)));
    }

    // -- Pairing service --------------------------------------------------

    struct Rig {
        pairing: Pairing,
        clock: Arc<TestClock>,
        card: SystemCardHandle,
        agents: SharedAgents,
        path: PathBuf,
        _dir: tempfile::TempDir,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.toml");
        let clock = TestClock::new();
        let card = SystemCardHandle::default();
        let agents = SharedAgents::default();
        let pairing = Pairing::new(
            PairingState::new(counting_words()),
            clock.clone(),
            agents.clone(),
            Some(path.clone()),
            card.clone(),
            50051,
        );
        pairing.binds(&["127.0.0.1:9090".parse().unwrap()]);
        Rig {
            pairing,
            clock,
            card,
            agents,
            path,
            _dir: dir,
        }
    }

    fn pair_body(agent: &str, code: &str, admin: bool) -> Vec<u8> {
        serde_json::json!({"agent": agent, "code": code, "admin": admin})
            .to_string()
            .into_bytes()
    }

    fn json(resp: &Response) -> serde_json::Value {
        serde_json::from_slice(&resp.body).unwrap()
    }

    fn card_lines(rig: &Rig) -> Option<(SystemCardKind, Vec<String>)> {
        rig.card
            .frame_state(rig.clock.wall_us())
            .model
            .map(|m| (m.kind, m.lines))
    }

    #[test]
    fn firewall_warning_line_only_when_blocked() {
        use crate::firewall::{BlockReason, TailnetInbound};
        let lines = |f: Option<&TailnetInbound>| pairing_lines("111111", "http://x/pair".into(), f);
        let blocked = TailnetInbound::Blocked {
            reason: BlockReason::NoAllowRule,
            rule: None,
        };
        assert_eq!(lines(None).len(), 3);
        for quiet in [
            TailnetInbound::Allowed,
            TailnetInbound::NotApplicable,
            TailnetInbound::Unknown { error: "e".into() },
        ] {
            assert_eq!(lines(Some(&quiet)).len(), 3, "{quiet:?}");
        }
        let warned = lines(Some(&blocked));
        assert_eq!(warned.len(), 4);
        assert!(warned[3].contains("Windows Firewall"));
        assert_eq!(warned[..3], lines(None)[..]);
    }

    #[test]
    fn a_blocked_tailnet_address_adds_the_warning_to_the_card_without_waiting() {
        use crate::firewall::{BlockReason, FirewallProbe, TailnetInbound};
        let mut rig = rig();
        {
            let mut binds = rig.pairing.binds.lock().unwrap();
            binds.clear();
            binds.push("100.100.1.2:9090".parse().unwrap());
        }
        rig.pairing.firewall = Arc::new(FirewallProbe::new(|| TailnetInbound::Blocked {
            reason: BlockReason::NoAllowRule,
            rule: None,
        }));
        rig.pairing.open();
        // Shown at once without the warning; the background check adds it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while card_lines(&rig).unwrap().1.len() < 4 {
            assert!(
                std::time::Instant::now() < deadline,
                "warning never appeared"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let (_, lines) = card_lines(&rig).unwrap();
        assert_eq!(lines[0], "111111");
        assert_eq!(lines[1], "http://100.100.1.2:9090/pair");
    }

    #[test]
    fn the_right_code_pairs_once_and_stores_only_the_hash() {
        let rig = rig();
        rig.pairing.open();
        let (kind, lines) = card_lines(&rig).expect("card while pairing");
        assert_eq!(kind, SystemCardKind::Pairing);
        assert_eq!(lines[0], "111111");
        assert_eq!(lines[1], "http://127.0.0.1:9090/pair");

        let ok = rig.pairing.pair(&pair_body("claude", "111111", false));
        assert_eq!(ok.status, 200);
        let body = json(&ok);
        let psk = body["psk"].as_str().unwrap();
        assert_eq!(psk.len(), 64);
        assert_eq!(body["agent"], "claude");
        assert_eq!(body["mcp"], "http://127.0.0.1:9090/mcp");
        assert_eq!(body["grpc"], "127.0.0.1:50051");

        // The PSK authenticates; agents.toml holds the hash, not the PSK.
        assert!(rig.agents.load().resolve(psk, "").is_ok());
        let saved = std::fs::read_to_string(&rig.path).unwrap();
        assert!(!saved.contains(psk));
        assert!(saved.contains(&agents_file::hash_psk_hex(psk)));
        assert!(saved.contains("paired_at = \"1970-01-01T00:00:01Z\""));

        // The card is replaced by the toast, and the code is spent.
        let (kind, _) = card_lines(&rig).unwrap();
        assert_eq!(kind, SystemCardKind::Toast);
        let again = rig.pairing.pair(&pair_body("claude", "111111", false));
        assert_eq!(again.status, 403);
        assert!(String::from_utf8_lossy(&again.body).contains("PAIRING_CLOSED"));
    }

    #[test]
    fn admin_is_opt_in_and_star_does_not_grant_it() {
        let rig = rig();
        rig.pairing.open();
        let plain = json(&rig.pairing.pair(&pair_body("plain", "111111", false)));
        rig.pairing.open();
        let admin = json(&rig.pairing.pair(&pair_body("boss", "222222", true)));
        let dir = rig.agents.load();
        let plain = dir.resolve(plain["psk"].as_str().unwrap(), "").unwrap();
        let admin = dir.resolve(admin["psk"].as_str().unwrap(), "").unwrap();
        assert!(!plain.is_operator_admin());
        assert!(admin.is_operator_admin());
        let saved = std::fs::read_to_string(&rig.path).unwrap();
        assert!(saved.contains("allow = [\"*\", \"admin\"]"));
    }

    #[test]
    fn re_pairing_an_id_rotates_its_key() {
        let rig = rig();
        rig.pairing.open();
        let first = json(&rig.pairing.pair(&pair_body("claude", "111111", false)));
        rig.pairing.open();
        let second = json(&rig.pairing.pair(&pair_body("claude", "222222", false)));
        let dir = rig.agents.load();
        assert!(dir.resolve(first["psk"].as_str().unwrap(), "").is_err());
        assert!(dir.resolve(second["psk"].as_str().unwrap(), "").is_ok());
    }

    #[test]
    fn wrong_code_is_rejected_and_pairing_stays_closed_until_asked() {
        let rig = rig();
        // Never opened (agents exist, no --pair): closed.
        let closed = rig.pairing.pair(&pair_body("claude", "111111", false));
        assert_eq!(closed.status, 403);
        assert!(String::from_utf8_lossy(&closed.body).contains("PAIRING_CLOSED"));
        assert!(card_lines(&rig).is_none());

        rig.pairing.open();
        let wrong = rig.pairing.pair(&pair_body("claude", "999999", false));
        assert_eq!(wrong.status, 403);
        assert!(String::from_utf8_lossy(&wrong.body).contains("PAIR_CODE_INVALID"));
        assert!(rig.agents.load().is_empty());
    }

    #[test]
    fn expired_code_is_closed_and_the_card_self_expires() {
        let rig = rig();
        rig.pairing.open();
        rig.clock.advance(CODE_TTL + Duration::from_millis(1));
        let resp = rig.pairing.pair(&pair_body("claude", "111111", false));
        assert!(String::from_utf8_lossy(&resp.body).contains("PAIRING_CLOSED"));
        assert!(card_lines(&rig).is_none(), "card clears with the code");
    }

    #[test]
    fn replacement_updates_the_card_and_cooldown_clears_it() {
        let rig = rig();
        rig.pairing.open();
        for _ in 0..MAX_BAD_ATTEMPTS {
            rig.pairing.pair(&pair_body("claude", "000000", false));
        }
        assert_eq!(card_lines(&rig).unwrap().1[0], "222222");
        for _ in 0..MAX_BAD_ATTEMPTS * 2 {
            rig.pairing.pair(&pair_body("claude", "000000", false));
        }
        assert!(card_lines(&rig).is_none());
        // Opening during the cooldown does not issue a code.
        rig.pairing.open();
        assert_ne!(card_lines(&rig).unwrap().0, SystemCardKind::Pairing);
    }

    #[test]
    fn bad_ids_and_bodies_are_400_and_do_not_burn_attempts() {
        let rig = rig();
        rig.pairing.open();
        for body in [
            pair_body("Claude", "111111", false),
            pair_body("", "111111", false),
            pair_body(&"a".repeat(33), "111111", false),
            pair_body("a_b", "111111", false),
            b"not json".to_vec(),
            br#"{"agent":"a","code":"111111","extra":1}"#.to_vec(),
        ] {
            assert_eq!(rig.pairing.pair(&body).status, 400);
        }
        assert_eq!(
            rig.pairing.pair(&pair_body("a-1", "111111", false)).status,
            200
        );
    }

    #[test]
    fn a_failed_save_keeps_pairing_usable_with_a_new_code() {
        let rig = rig();
        std::fs::create_dir(&rig.path).unwrap(); // agents.toml is a directory
        rig.pairing.open();
        let resp = rig.pairing.pair(&pair_body("claude", "111111", false));
        assert_eq!(resp.status, 503);
        assert!(rig.agents.load().is_empty());
        assert_eq!(card_lines(&rig).unwrap().1[0], "222222");
    }

    #[test]
    fn rfc3339_formats_known_instants() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_709_210_096_000_000), "2024-02-29T12:34:56Z");
    }

    /// Everything tracing emits in this test process.
    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    /// A process-global capture, installed once. A per-thread subscriber is
    /// unreliable here: tracing caches callsite interest process-wide, so a
    /// parallel test hitting the same callsite with no subscriber can disable
    /// it for the capturing thread. Global, it also sees every other pairing
    /// test's logs, which only widens the leak check.
    fn global_capture() -> &'static LogCapture {
        static CAPTURE: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
        CAPTURE.get_or_init(|| {
            let log = LogCapture::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_writer(log.clone())
                    // Timestamps carry digit runs that can look like a code.
                    .without_time()
                    .with_max_level(tracing::Level::TRACE)
                    .finish(),
            )
            .expect("no other test installs a global subscriber");
            log
        })
    }

    #[test]
    fn logs_contain_neither_the_code_nor_the_psk() {
        let log = global_capture();
        let rig = rig();
        rig.pairing.open();
        // Wrong guesses (replacement, then cooldown), a spent code, then a
        // fresh opening that succeeds.
        for _ in 0..MAX_BAD_ATTEMPTS * 3 {
            rig.pairing.pair(&pair_body("claude", "000000", false));
        }
        rig.clock.advance(COOLDOWN);
        rig.pairing.open(); // code 444444
        let ok = json(&rig.pairing.pair(&pair_body("claude", "444444", true)));
        rig.pairing.pair(&pair_body("claude", "444444", true));
        let psk = ok["psk"].as_str().unwrap();
        let logged = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("agent paired"), "capture is live: {logged}");
        for secret in [psk, "111111", "222222", "333333", "444444"] {
            assert!(!logged.contains(secret), "log leaks {secret}");
        }
    }

    #[tokio::test]
    async fn pair_over_http_then_use_the_psk_for_mcp() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let rig = rig();
        let pairing = Arc::new(rig.pairing);
        let shutdown = crate::threads::ShutdownToken::new();
        let config = crate::mcp::McpServerConfig {
            bind_addrs: vec!["127.0.0.1:0".parse().unwrap()],
            late_tailnet_port: None,
            agents: rig.agents.clone(),
            presents: None,
            capture: None,
            restart: None,
            bind_gate: None,
            update: None,
            pairing: Some(Arc::clone(&pairing)),
        };
        let scene = Arc::new(tokio::sync::Mutex::new(
            tze_hud_scene::graph::SceneGraph::new(1920.0, 1080.0),
        ));
        let (_handle, addrs) =
            crate::mcp::start_mcp_http_server(scene, config, shutdown.clone(), None)
                .await
                .unwrap();
        let addr = addrs[0];

        let request = |path: &'static str, bearer: Option<String>, body: String| async move {
            let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
            let auth = bearer
                .map(|t| format!("Authorization: Bearer {t}\r\n"))
                .unwrap_or_default();
            let req = format!(
                "POST {path} HTTP/1.0\r\n{auth}Content-Length: {}\r\n\r\n{body}",
                body.len()
            );
            conn.write_all(req.as_bytes()).await.unwrap();
            let mut resp = String::new();
            conn.read_to_string(&mut resp).await.unwrap();
            let (head, body) = resp.split_once("\r\n\r\n").unwrap();
            (head.to_owned(), body.to_owned())
        };
        let pair_json = |code: &str| format!(r#"{{"agent":"claude","code":"{code}"}}"#);

        // Closed before anyone opens it, and no bearer is needed to ask.
        let (head, body) = request("/pair", None, pair_json("111111")).await;
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        assert!(body.contains("PAIRING_CLOSED"));

        pairing.open();
        let (head, body) = request("/pair", None, pair_json("999999")).await;
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        assert!(body.contains("PAIR_CODE_INVALID"));

        let (head, body) = request("/pair", None, pair_json("111111")).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let psk = serde_json::from_str::<serde_json::Value>(&body).unwrap()["psk"]
            .as_str()
            .unwrap()
            .to_owned();

        let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_owned();
        let (_, body) = request("/mcp", Some(psk), list.clone()).await;
        assert!(body.contains("hud_publish"), "{body}");
        let (_, body) = request("/mcp", Some("wrong".into()), list).await;
        assert!(!body.contains("hud_publish"), "{body}");

        let (head, _) = request("/pair", None, pair_json("111111")).await;
        assert!(head.starts_with("HTTP/1.1 403"));
        shutdown.trigger(crate::threads::ShutdownReason::Clean);
    }
}
