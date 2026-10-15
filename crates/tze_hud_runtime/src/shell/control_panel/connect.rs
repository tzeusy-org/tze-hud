//! Connect an agent (CP-13): a setup prompt the owner copies into any agent
//! session. The prompt carries a one-time pairing code, never a key; the
//! agent redeems the code at `POST /pair` and keeps the key it gets back.

use std::sync::Arc;
use std::time::Duration;

use tze_hud_compositor::PanelTone;

use super::row::{Control, Row};
use crate::agent_store::AgentStore;
use crate::firewall::TailnetInbound;
use crate::pairing::{Pairing, Reach};

/// Writes text to the clipboard (injected so tests can read it back).
pub(crate) type Clipboard = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// The Agents row (and quick tile) that opens the card.
pub(crate) const OPEN: &str = "agents.connect";

/// The rows the card shows beside the prompt. `NAME` takes typing.
pub(crate) const NAME: &str = "connect.name";
pub(crate) const CODE: &str = "connect.code";
pub(crate) const REACH: &str = "connect.reach";
pub(crate) const NETWORK: &str = "connect.network";
pub(crate) const GRANTS: &str = "connect.grants";
pub(crate) const COPY: &str = "connect.copy";
pub(crate) const NEW_CODE: &str = "connect.new_code";

/// The longest agent name `POST /pair` accepts.
const NAME_MAX: usize = 32;
/// A shown code with less time left is replaced before copying: the agent
/// needs time to read the prompt and call `/pair`.
const MIN_CODE_LEFT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub(crate) struct ConnectControl {
    pub pairing: Arc<Pairing>,
    pub store: Arc<AgentStore>,
    pub clipboard: Clipboard,
    /// This PC's name, for the prompt (`COMPUTERNAME`).
    pub machine: Option<String>,
}

/// The card while it is open: the name being typed and what the last action
/// did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ConnectPage {
    pub name: String,
    /// The name is the suggested one, untouched: typing replaces it, as in a
    /// field whose text is selected.
    pub suggested: bool,
    pub status: Option<ConnectStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConnectStatus {
    /// The prompt is on the clipboard; `before` is the agent's `paired_at`
    /// at that moment, so a (re-)pairing shows as a change.
    Copied {
        agent: String,
        before: Option<String>,
        /// The code in the copied prompt.
        code: String,
    },
    Failed(String),
}

impl ConnectPage {
    /// Type into the name: lower case, digits and dashes, as `POST /pair`
    /// takes them.
    pub(crate) fn erase(&mut self) {
        if std::mem::take(&mut self.suggested) {
            self.name.clear();
        } else {
            self.name.pop();
        }
    }

    pub(crate) fn type_name(&mut self, text: &str) {
        if std::mem::take(&mut self.suggested) {
            self.name.clear();
        }
        for c in text.chars() {
            let c = match c.to_ascii_lowercase() {
                ' ' | '_' => '-',
                c => c,
            };
            if (c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && self.name.len() < NAME_MAX
            {
                self.name.push(c);
            }
        }
    }
}

/// Everything the card shows, read fresh on each build.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConnectView {
    pub prompt: Vec<String>,
    pub rows: Vec<Row>,
    pub status: Option<(String, PanelTone)>,
    pub banners: Vec<String>,
}

fn valid_name(name: &str) -> bool {
    (1..=NAME_MAX).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `564016` → `564 016`.
fn spaced(code: &str) -> String {
    let (a, b) = code.split_at(code.len() / 2);
    format!("{a} {b}")
}

fn clock(left: Duration) -> String {
    let secs = left.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The setup prompt. Holds the one-time `code`, never a key.
pub(crate) fn prompt(
    machine: Option<&str>,
    reach: &Reach,
    name: &str,
    code: &str,
    left: Duration,
) -> Vec<String> {
    let at = reach.endpoint;
    let host = at.ip();
    let on = machine.map_or_else(String::new, |m| format!(" ({m})"));
    let mut lines = vec![format!(
        "Connect to my tze_hud HUD, a screen overlay on my Windows PC{on}."
    )];
    if reach.tailnet {
        lines.extend([
            format!("It is reachable ONLY inside my Tailscale tailnet at {at}."),
            "There is no public or LAN access. If you cannot reach that address from".to_owned(),
            "your tailnet, stop and tell me; do not look for another route.".to_owned(),
        ]);
    } else {
        lines.extend([
            format!("It is reachable ONLY from that PC itself, at {at}."),
            "If you are not running on that PC, stop and tell me; do not look for".to_owned(),
            "another route.".to_owned(),
        ]);
    }
    let minutes = left.as_secs().div_ceil(60).max(1);
    lines.extend([
        String::new(),
        format!(
            "1. Pair once. The code works one time and expires within {minutes} minute{}:",
            if minutes == 1 { "" } else { "s" }
        ),
        "   This saves the key (psk) to a 0600 file and prints only the rest:".to_owned(),
        format!(r#"   curl -s http://{at}/pair -d '{{"agent":"{name}","code":"{code}"}}' |"#),
        format!(
            r#"   python3 -c 'import json,os,sys; r=json.load(sys.stdin); k=r.pop("psk",None) or sys.exit(json.dumps(r)); os.umask(0o077); d=os.path.expanduser("~/.config/tze-hud"); os.makedirs(d,exist_ok=True); p=d+"/{host}.psk"; open(p,"w").write(k+"\n"); os.chmod(p,0o600); print(r)'"#
        ),
        "   Never print, log or commit the key. A 403 PAIRING_CLOSED means ask me".to_owned(),
        "   for a new code.".to_owned(),
        String::new(),
        "2. Use MCP over HTTP at the returned mcp URL".to_owned(),
        format!("   (http://{at}/mcp) with the header"),
        "   Authorization: Bearer <psk>. Call hud_surfaces first, then".to_owned(),
        "   hud_publish, hud_input, hud_hold and hud_clear.".to_owned(),
        String::new(),
        "3. Send content and typed values only. The HUD decides layout, styling".to_owned(),
        "   and timing. Every error has a code and a hint; follow the hint.".to_owned(),
    ]);
    lines
}

/// What the Network row and the banners say about reachability.
fn network(reach: &Reach) -> (String, PanelTone, Option<String>) {
    if !reach.tailnet {
        return (
            "This PC only · Tailscale isn't up".to_owned(),
            PanelTone::Caution,
            Some(
                "Tailscale isn't up: agents can only connect from this PC. \
                 The prompt says so and uses this PC's address."
                    .to_owned(),
            ),
        );
    }
    match &reach.firewall {
        Some(TailnetInbound::Blocked { .. }) => (
            "Tailnet only · Windows Firewall blocks it".to_owned(),
            PanelTone::Caution,
            Some(format!(
                "Windows Firewall blocks tailnet agents: {}. Until then the prompt won't work.",
                crate::firewall::FIX_HINT
            )),
        ),
        Some(TailnetInbound::Unknown { .. }) => (
            "Tailnet only · firewall state unknown".to_owned(),
            PanelTone::Muted,
            None,
        ),
        Some(_) => (
            "Tailnet only · firewall allows it".to_owned(),
            PanelTone::Success,
            None,
        ),
        None => (
            "Tailnet only · checking the firewall…".to_owned(),
            PanelTone::Muted,
            None,
        ),
    }
}

impl ConnectControl {
    /// A name no paired agent has, so copying never re-keys one by accident.
    pub(crate) fn fresh_name(&self) -> String {
        let paired = self.store.cached();
        std::iter::once("agent".to_owned())
            .chain((2..).map(|n| format!("agent-{n}")))
            .find(|name| !paired.agents.contains_key(name))
            .expect("an unbounded sequence has a free name")
    }

    /// Open the card: a fresh one-time code that only the card shows.
    pub(crate) fn open(&self) -> ConnectPage {
        self.pairing.open_in_panel();
        ConnectPage {
            name: self.fresh_name(),
            suggested: true,
            status: None,
        }
    }

    /// The card is gone (Esc back, or the panel closed).
    pub(crate) fn leave(&self) {
        self.pairing.leave_panel();
    }

    pub(crate) fn new_code(&self, page: &mut ConnectPage) {
        if self.pairing.open_in_panel().is_none() {
            page.status = Some(ConnectStatus::Failed(
                "Too many wrong codes; try again in a minute.".to_owned(),
            ));
        } else {
            page.status = None;
        }
    }

    /// Put the prompt on the clipboard, issuing a fresh code first when the
    /// shown one is spent or expired.
    pub(crate) async fn copy(&self, page: &mut ConnectPage) {
        if !valid_name(&page.name) {
            page.status = Some(ConnectStatus::Failed(
                "Name the agent first: lower case, digits and dashes.".to_owned(),
            ));
            return;
        }
        let (code, left) = match self.pairing.code() {
            Ok((code, left)) if left >= MIN_CODE_LEFT => (code, left),
            _ => {
                self.new_code(page);
                match self.pairing.code() {
                    Ok(code) => code,
                    Err(_) => return,
                }
            }
        };
        let text = prompt(
            self.machine.as_deref(),
            &self.pairing.reach(),
            &page.name,
            &code,
            left,
        )
        .join("\n");
        let clipboard = Arc::clone(&self.clipboard);
        let written = tokio::task::spawn_blocking(move || clipboard(&text))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        page.status = Some(match written {
            Ok(()) => {
                tracing::info!(agent = %page.name, "control panel: connect prompt copied");
                ConnectStatus::Copied {
                    agent: page.name.clone(),
                    before: self.paired_at(&page.name),
                    code,
                }
            }
            Err(error) => {
                tracing::warn!(%error, "control panel: copying the connect prompt failed");
                ConnectStatus::Failed(format!("Couldn't copy: {error}"))
            }
        });
    }

    fn paired_at(&self, agent: &str) -> Option<String> {
        self.store
            .cached()
            .agents
            .get(agent)
            .map(|r| r.paired_at.clone().unwrap_or_default())
    }

    /// The card as it stands now. Takes only short locks (the pairing
    /// state, the agent cache) and reads the interface list at most every
    /// few seconds; the firewall check runs in the background.
    pub(crate) fn view(&self, page: &ConnectPage) -> ConnectView {
        let reach = self.pairing.reach();
        let code = self.pairing.code();
        let name = if page.name.is_empty() {
            "<name>"
        } else {
            &page.name
        };
        let (code_text, code_detail, shown_code, left) = match &code {
            Ok((code, left)) => (
                spaced(code),
                format!("expires in {} · works once", clock(*left)),
                code.clone(),
                *left,
            ),
            Err(true) => (
                "—".to_owned(),
                "Too many wrong codes; try again in a minute".to_owned(),
                "<code>".to_owned(),
                Duration::ZERO,
            ),
            Err(false) => (
                "—".to_owned(),
                "Used or expired · Copy issues a new one".to_owned(),
                "<code>".to_owned(),
                Duration::ZERO,
            ),
        };
        let (network_text, network_tone, banner) = network(&reach);
        let banners: Vec<String> = banner.into_iter().collect();
        let rows = vec![
            Row::new(
                NAME,
                "Agent name",
                Control::Field {
                    text: page.name.clone(),
                },
            )
            .detail("Lower case, digits and dashes. An existing name gets a new key."),
            Row::new(
                CODE,
                "One-time code",
                Control::ReadOnly {
                    text: code_text,
                    mono: true,
                },
            )
            .detail(code_detail),
            Row::new(
                REACH,
                "Reachable at",
                Control::ReadOnly {
                    text: reach.endpoint.to_string(),
                    mono: true,
                },
            ),
            Row::new(
                NETWORK,
                "Network",
                Control::ReadOnly {
                    text: String::new(),
                    mono: false,
                },
            )
            .detail(network_text)
            .status(network_tone),
            Row::new(
                GRANTS,
                "Grants",
                Control::ReadOnly {
                    text: String::new(),
                    mono: false,
                },
            )
            .detail("Zones, widgets, portal, tiles. Not admin."),
            Row::new(
                COPY,
                "Copy prompt · Enter",
                Control::Action { confirm: None },
            )
            .detail("Holds the one-time code, never a key; the HUD keeps only a hash"),
            Row::new(NEW_CODE, "New code", Control::Action { confirm: None })
                .detail("The current code stops working"),
        ];
        let status = page.status.as_ref().map(|status| match status {
            ConnectStatus::Copied {
                agent,
                before,
                code: copied,
            } => {
                if self.paired_at(agent) != *before {
                    (
                        format!("{agent} paired. It shows in Agents."),
                        PanelTone::Success,
                    )
                } else if !matches!(&code, Ok((live, _)) if live == copied) {
                    (
                        "The copied code no longer works. Enter copies a new one.".to_owned(),
                        PanelTone::Caution,
                    )
                } else {
                    (
                        format!("Copied. Waiting for {agent} to pair…"),
                        PanelTone::Success,
                    )
                }
            }
            ConnectStatus::Failed(why) => (why.clone(), PanelTone::Danger),
        });
        ConnectView {
            prompt: prompt(self.machine.as_deref(), &reach, name, &shown_code, left),
            rows,
            status,
            banners,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reach(endpoint: &str, tailnet: bool) -> Reach {
        Reach {
            endpoint: endpoint.parse::<std::net::SocketAddr>().unwrap(),
            tailnet,
            firewall: Some(TailnetInbound::Allowed),
        }
    }

    /// The prompt (CP-13) restricts the agent to the tailnet address, carries
    /// the name and one-time code for `POST /pair`, says where the key goes
    /// and how to use it, and names the five MCP verbs.
    #[test]
    fn prompt_pins_the_tailnet_route_and_carries_only_the_code() {
        let r = reach("100.87.181.125:9290", true);
        let text = prompt(
            Some("tzehouse-windows"),
            &r,
            "claude-laptop",
            "564016",
            Duration::from_secs(292),
        )
        .join("\n");
        for needle in [
            "(tzehouse-windows)",
            "reachable ONLY inside my Tailscale tailnet at 100.87.181.125:9290",
            "stop and tell me; do not look for another route",
            "expires within 5 minutes",
            "curl -s http://100.87.181.125:9290/pair",
            r#"{"agent":"claude-laptop","code":"564016"}"#,
            "saves the key (psk) to a 0600 file",
            r#"d+"/100.87.181.125.psk""#,
            "Never print, log or commit the key",
            "Authorization: Bearer <psk>",
            "http://100.87.181.125:9290/mcp",
            "hud_surfaces",
            "hud_publish, hud_input, hud_hold and hud_clear",
            "follow the hint",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
    }

    /// Running the prompt's save step on a pair reply writes the key to a
    /// private file and prints everything but the key; an error reply is
    /// printed whole (it holds no key).
    #[cfg(unix)]
    #[test]
    fn the_prompts_save_step_stores_the_key_without_printing_it() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};
        let text = prompt(
            None,
            &reach("100.64.0.1:9090", true),
            "a",
            "123456",
            Duration::from_secs(300),
        );
        let line = text.iter().find(|l| l.contains("python3 -c")).unwrap();
        let script = line
            .trim()
            .strip_prefix("python3 -c '")
            .unwrap()
            .strip_suffix('\'')
            .unwrap();
        let run = |home: &std::path::Path, reply: &str| {
            let mut child = Command::new("python3")
                .args(["-c", script])
                .env("HOME", home)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("python3 runs the prompt's save step");
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(reply.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        };
        // A key file left readable by an older setup is tightened too.
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join(".config/tze-hud/100.64.0.1.psk");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "OLD\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let out = run(
            home.path(),
            r#"{"agent":"a","psk":"SECRET-KEY","mcp":"http://m/mcp","grpc":"g:1"}"#,
        );
        assert!(out.status.success());
        let shown = String::from_utf8_lossy(&out.stdout);
        assert!(
            shown.contains("http://m/mcp") && !shown.contains("SECRET"),
            "{shown}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "SECRET-KEY\n");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let out = run(home.path(), r#"{"code":"PAIRING_CLOSED","hint":"ask"}"#);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("PAIRING_CLOSED"));
    }

    /// With Tailscale down the prompt says the HUD is reachable only from this
    /// PC and names the loopback address.
    #[test]
    fn prompt_without_tailscale_is_this_pc_only() {
        let r = reach("127.0.0.1:9090", false);
        let text = prompt(None, &r, "a", "000001", Duration::from_secs(30)).join("\n");
        assert!(text.contains("reachable ONLY from that PC itself, at 127.0.0.1:9090"));
        assert!(!text.contains("tailnet"));
        assert!(text.contains("expires within 1 minute:"));
        assert!(text.starts_with("Connect to my tze_hud HUD, a screen overlay on my Windows PC."));
    }

    #[test]
    fn names_type_as_pair_accepts_them() {
        let mut page = ConnectPage {
            name: "agent".into(),
            suggested: true,
            status: None,
        };
        page.type_name("Claude Laptop_2!");
        assert_eq!(page.name, "claude-laptop-2");
        assert!(valid_name(&page.name));
        page.erase();
        assert_eq!(
            page.name, "claude-laptop-",
            "an edited name erases a character"
        );
        page.type_name(&"x".repeat(40));
        assert_eq!(page.name.len(), NAME_MAX);
        let mut page = ConnectPage {
            name: "agent".into(),
            suggested: true,
            status: None,
        };
        page.erase();
        assert_eq!(page.name, "", "erasing the suggestion clears it");
        assert!(!valid_name(""));
    }

    #[test]
    fn network_row_and_banners_follow_reachability() {
        let mut r = reach("100.64.0.1:9090", true);
        assert_eq!(network(&r).1, PanelTone::Success);
        r.firewall = None;
        assert!(network(&r).0.contains("checking"));
        r.firewall = Some(TailnetInbound::Blocked {
            reason: crate::firewall::BlockReason::NoAllowRule,
            rule: None,
        });
        let (_, tone, banner) = network(&r);
        assert_eq!(tone, PanelTone::Caution);
        assert!(
            banner
                .unwrap()
                .contains("Windows Firewall blocks tailnet agents")
        );
        let down = reach("127.0.0.1:9090", false);
        assert!(network(&down).2.unwrap().starts_with("Tailscale isn't up"));
    }

    /// Copy replaces a spent, expired or nearly expired code before it
    /// writes the prompt, so the agent never gets a code that cannot work.
    #[tokio::test]
    async fn copy_issues_a_fresh_code_when_the_shown_one_cannot_work() {
        let rig = crate::pairing::tests::rig();
        let copied = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&copied);
        let control = ConnectControl {
            pairing: Arc::clone(&rig.pairing),
            store: Arc::clone(&rig.store),
            clipboard: Arc::new(move |text: &str| {
                sink.lock().unwrap().push(text.to_owned());
                Ok(())
            }),
            machine: None,
        };
        let code_in = |text: &str| {
            let at = text.find(r#""code":""#).unwrap() + 8;
            text[at..at + 6].to_owned()
        };
        let mut page = control.open();
        let shown = rig.pairing.code().unwrap().0;
        control.copy(&mut page).await;
        assert_eq!(
            code_in(&copied.lock().unwrap()[0]),
            shown,
            "a live code is kept"
        );

        // Expired.
        rig.clock
            .advance(crate::pairing::CODE_TTL + Duration::from_secs(1));
        control.copy(&mut page).await;
        let fresh = code_in(&copied.lock().unwrap()[1]);
        assert_ne!(fresh, shown);
        assert_eq!(
            rig.pairing.code().unwrap().0,
            fresh,
            "the copied code is live"
        );

        // Under a minute left.
        rig.clock
            .advance(crate::pairing::CODE_TTL - Duration::from_secs(30));
        control.copy(&mut page).await;
        let renewed = code_in(&copied.lock().unwrap()[2]);
        assert_ne!(renewed, fresh);
        assert!(rig.pairing.code().unwrap().1 > MIN_CODE_LEFT);

        // Spent by an agent pairing.
        let body = serde_json::json!({"agent": page.name, "code": renewed}).to_string();
        assert_eq!(rig.pairing.pair(body.as_bytes()).status, 200);
        assert!(control.view(&page).status.unwrap().0.contains("paired"));
        control.copy(&mut page).await;
        assert_ne!(code_in(&copied.lock().unwrap()[3]), renewed);

        // Wrong guesses replace the copied code: the card says so.
        let status = |page: &ConnectPage| control.view(page).status.unwrap().0;
        assert!(status(&page).starts_with("Copied."));
        for _ in 0..crate::pairing::MAX_BAD_ATTEMPTS {
            rig.pairing.pair(br#"{"agent":"x","code":"000000"}"#);
        }
        assert!(rig.pairing.code().is_ok(), "a replacement is open");
        assert!(
            status(&page).contains("no longer works"),
            "{}",
            status(&page)
        );
    }

    /// A clipboard failure says so on the card instead of claiming a copy.
    #[tokio::test]
    async fn a_failed_copy_says_so() {
        let rig = crate::pairing::tests::rig();
        let control = ConnectControl {
            pairing: Arc::clone(&rig.pairing),
            store: Arc::clone(&rig.store),
            clipboard: Arc::new(|_: &str| Err("clipboard is busy".to_owned())),
            machine: None,
        };
        let mut page = control.open();
        control.copy(&mut page).await;
        let (text, tone) = control.view(&page).status.unwrap();
        assert_eq!(text, "Couldn't copy: clipboard is busy");
        assert_eq!(tone, PanelTone::Danger);
    }
}
