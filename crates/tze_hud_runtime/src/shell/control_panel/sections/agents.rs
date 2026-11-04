//! Agents: pause and resume every agent, and one row per agent with its
//! connection and holdings. Opening an agent's row shows Clear, Unpair and
//! its allow-list grants (CP-8).

use std::sync::Mutex;

use tze_hud_compositor::PanelTone;
use tze_hud_config::{DEFAULT_ANSWER_WINDOW_MS, Grant, RuntimeUpdate};

use super::super::runtime::RuntimeControl;
use crate::launch::Source;

use super::super::agents::{AgentControl, AgentEntry, Cleared};
use super::super::pause::PauseControl;
use super::super::row::{BoxFuture, Control, Edit, Row, Section};

const ANSWER_CHOICES: [u32; 5] = [15_000, 30_000, 60_000, 120_000, 300_000];

pub(crate) struct AgentsSection {
    pause: PauseControl,
    runtime: Option<RuntimeControl>,
    /// `None` when the HUD has no agent services (no network runtime).
    agents: Option<AgentControl>,
    /// The agent whose actions are showing.
    expanded: Mutex<Option<String>>,
    /// The roster as last read: reading it takes the session, scene and
    /// portal locks, so rows read this and `refresh` renews it.
    roster: Mutex<Vec<AgentEntry>>,
    /// Offer Connect an agent (the panel task runs it).
    connect: bool,
}

/// What a per-agent row does. Row ids are `agents.<verb>:<agent>`, so any
/// namespace (dev ones may contain dots) parses back unambiguously.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verb {
    Manage,
    Clear,
    Unpair,
    Grant(Grant),
}

impl Verb {
    fn name(self) -> &'static str {
        match self {
            Verb::Manage => "manage",
            Verb::Clear => "clear",
            Verb::Unpair => "unpair",
            Verb::Grant(Grant::Zones) => "grant.zones",
            Verb::Grant(Grant::Widgets) => "grant.widgets",
            Verb::Grant(Grant::Portal) => "grant.portal",
            Verb::Grant(Grant::Admin) => "grant.admin",
        }
    }

    fn all() -> impl Iterator<Item = Verb> {
        [Verb::Manage, Verb::Clear, Verb::Unpair]
            .into_iter()
            .chain(Grant::ALL.into_iter().map(Verb::Grant))
    }

    fn row(self, agent: &str) -> String {
        format!("agents.{}:{agent}", self.name())
    }

    fn parse(row: &str) -> Option<(Verb, &str)> {
        let (name, agent) = row.strip_prefix("agents.")?.split_once(':')?;
        Verb::all().find(|v| v.name() == name).map(|v| (v, agent))
    }
}

fn count(n: usize, one: &str, many: &str) -> Option<String> {
    match n {
        0 => None,
        1 => Some(format!("1 {one}")),
        n => Some(format!("{n} {many}")),
    }
}

/// The header's state line: agents connected or showing something, and
/// what all of them hold ("3 agents · 2 portals · 5 zones · 1 widget ·
/// 0 tiles"); every count shows, zeros included, so the line does not
/// change shape.
fn state_line(roster: &[AgentEntry]) -> String {
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let active = roster
        .iter()
        .filter(|e| e.sessions > 0 || !e.holds_nothing())
        .count();
    let sum = |f: fn(&AgentEntry) -> usize| roster.iter().map(f).sum::<usize>();
    [
        plural(active, "agent", "agents"),
        plural(sum(|e| e.portals), "portal", "portals"),
        plural(sum(|e| e.holdings.zones), "zone", "zones"),
        plural(sum(|e| e.holdings.widgets), "widget", "widgets"),
    ]
    .join(" · ")
}

/// "a portal, 2 zones and 1 widget", or `None` for nothing.
fn holdings_phrase(entry: &AgentEntry) -> Option<String> {
    let h = entry.holdings;
    let parts: Vec<String> = [
        count(entry.portals, "portal", "portals"),
        count(h.zones, "zone", "zones"),
        count(h.widgets, "widget", "widgets"),
    ]
    .into_iter()
    .flatten()
    .collect();
    match parts.as_slice() {
        [] => None,
        [one] => Some(one.clone()),
        [rest @ .., last] => Some(format!("{} and {last}", rest.join(", "))),
    }
}

fn status_line(entry: &AgentEntry) -> String {
    let holds = holdings_phrase(entry);
    let mut line = match (entry.sessions > 0, holds) {
        (true, Some(holds)) => format!("Connected · {holds}"),
        (true, None) => "Connected · holds nothing".to_owned(),
        (false, Some(holds)) => format!("Showing {holds}"),
        (false, None) => "Idle · holds nothing".to_owned(),
    };
    if !entry.paired {
        line.push_str(" · not paired");
    }
    line
}

fn grant_label(grant: Grant) -> &'static str {
    match grant {
        Grant::Zones => "Zones",
        Grant::Widgets => "Widgets",
        Grant::Portal => "Portal",
        Grant::Admin => "Admin",
    }
}

/// What the class lets the agent do, or which single surfaces it still has.
fn grant_detail(grant: Grant, entry: &AgentEntry, on: bool) -> String {
    let prefix = match grant {
        Grant::Zones => Some("zone:"),
        Grant::Widgets => Some("widget:"),
        _ => None,
    };
    let singles: Vec<&str> = prefix
        .map(|p| {
            entry
                .allow
                .iter()
                .filter_map(|e| e.strip_prefix(p))
                .filter(|n| *n != "*")
                .collect()
        })
        .unwrap_or_default();
    if !on && !singles.is_empty() {
        return format!("Only {}", singles.join(", "));
    }
    match grant {
        Grant::Zones => "Publish to every zone",
        Grant::Widgets => "Drive every widget",
        Grant::Portal => "Project a session portal",
        Grant::Admin => "HUD status and screenshots · widest",
    }
    .to_owned()
}

fn grant_question(grant: Grant, agent: &str) -> String {
    match grant {
        Grant::Admin => format!(
            "Give {agent} admin? It can read the HUD's status and take screenshots of this screen."
        ),
        g => format!(
            "Let {agent} use {}? It applies to its next request.",
            grant_label(g).to_lowercase()
        ),
    }
}

impl AgentsSection {
    pub(crate) const ANSWER_WINDOW: &'static str = "agents.answer_window";
    pub(crate) const PAUSE: &'static str = "agents.pause";

    pub(crate) fn new(
        pause: PauseControl,
        agents: Option<AgentControl>,
        connect: bool,
        runtime: Option<RuntimeControl>,
    ) -> Self {
        Self {
            pause,
            runtime,
            agents,
            expanded: Mutex::new(None),
            roster: Mutex::new(Vec::new()),
            connect,
        }
    }

    fn answer_row(&self) -> Option<Row> {
        let control = self.runtime.as_ref()?;
        let current = control.live.publication_defaults.answer_window_ms();
        let source = control.sources().answer_window_ms;
        let options = answer_options(current);
        let value = if source.is_pinned() {
            Control::ReadOnly {
                text: answer_label(current),
                mono: false,
            }
        } else {
            Control::Choice {
                selected: options.iter().position(|ms| *ms == current).unwrap_or(2),
                options: options.into_iter().map(answer_label).collect(),
            }
        };
        Some(
            Row::new(Self::ANSWER_WINDOW, "Questions wait for an answer", value)
                .detail(format!(
                    "How long a question card waits before it hands the question back · {}",
                    source.describe()
                ))
                .keywords(&["question", "choice", "answer", "timeout", "wait"])
                .resettable(
                    !source.is_pinned()
                        && (source == Source::Config || current != DEFAULT_ANSWER_WINDOW_MS),
                ),
        )
    }

    fn apply_answer(&self, edit: Edit) -> Result<(), String> {
        let control = self.runtime.as_ref().ok_or("No runtime settings")?;
        if control.sources().answer_window_ms.is_pinned() {
            return Err("The answer window is pinned at launch".into());
        }
        let ms = match edit {
            Edit::Choose(index) => {
                let current = control.live.publication_defaults.answer_window_ms();
                let ms = *answer_options(current)
                    .get(index)
                    .ok_or("No such answer window")?;
                control.save(RuntimeUpdate::AnswerWindowMs(Some(ms)), |s| {
                    &mut s.answer_window_ms
                })?;
                ms
            }
            Edit::Reset => {
                control.clear(RuntimeUpdate::AnswerWindowMs(None), |s| {
                    &mut s.answer_window_ms
                })?;
                DEFAULT_ANSWER_WINDOW_MS
            }
            _ => return Err("Choose an answer window or Reset".into()),
        };
        // A failed save leaves both the live value and its source untouched.
        control.live.publication_defaults.set_answer_window_ms(ms);
        Ok(())
    }

    fn expanded(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.expanded.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn reload(&self) {
        let Some(agents) = &self.agents else { return };
        let roster = agents.roster().await;
        {
            // An agent that left the roster takes its actions along.
            let mut expanded = self.expanded();
            if expanded
                .as_ref()
                .is_some_and(|id| !roster.iter().any(|e| e.id == *id))
            {
                *expanded = None;
            }
        }
        *self.roster.lock().unwrap_or_else(|e| e.into_inner()) = roster;
    }

    fn agent_rows(&self, entry: &AgentEntry, rows: &mut Vec<Row>) {
        const KEYWORDS: &[&str] = &[
            "agent",
            "clear",
            "unpair",
            "allow",
            "permissions",
            "revoke",
            "remove",
        ];
        let id = entry.id.as_str();
        let open = self.expanded().as_deref() == Some(id);
        let live = entry.sessions > 0 || !entry.holds_nothing();
        rows.push(
            Row::new(
                Verb::Manage.row(id),
                id,
                Control::Disclosure {
                    open,
                    text: if open {
                        "Done".into()
                    } else if entry.paired {
                        "Clear · Unpair · Allow".into()
                    } else {
                        "Clear".into()
                    },
                },
            )
            .detail(status_line(entry))
            .status(if live {
                PanelTone::Success
            } else {
                PanelTone::Muted
            })
            .keywords(KEYWORDS),
        );
        if !open {
            return;
        }
        let holds = holdings_phrase(entry);
        rows.push(
            Row::new(
                Verb::Clear.row(id),
                format!("Clear {id}"),
                Control::Action {
                    confirm: holds.as_ref().map(|h| {
                        format!("Clear {id}? Its {h} will disappear. It can publish again later.")
                    }),
                },
            )
            .detail(match &holds {
                Some(h) => format!("Remove its {h} now"),
                None => "It holds nothing".to_owned(),
            })
            .keywords(KEYWORDS),
        );
        if !entry.paired {
            return;
        }
        rows.push(
            Row::new(
                Verb::Unpair.row(id),
                format!("Unpair {id}"),
                Control::Action {
                    confirm: Some(format!(
                        "Unpair {id}? Its key stops working now and what it shows clears. \
                         Pair it again to reconnect."
                    )),
                },
            )
            .detail("Its key stops working; its content clears")
            .keywords(KEYWORDS),
        );
        for grant in Grant::ALL {
            let on = grant.granted(&entry.allow);
            rows.push(
                Row::new(
                    Verb::Grant(grant).row(id),
                    grant_label(grant),
                    Control::Grant {
                        on,
                        confirm: grant_question(grant, id),
                    },
                )
                .detail(grant_detail(grant, entry, on))
                .caution(grant == Grant::Admin && on)
                .keywords(KEYWORDS),
            );
        }
    }
}

fn answer_options(current: u32) -> Vec<u32> {
    let mut options = ANSWER_CHOICES.to_vec();
    if !options.contains(&current) {
        options.push(current);
        options.sort_unstable();
    }
    options
}

fn answer_label(ms: u32) -> String {
    if ms % 60_000 == 0 {
        format!("{} min", ms / 60_000)
    } else if ms % 1_000 == 0 {
        format!("{} s", ms / 1_000)
    } else {
        format!("{ms} ms")
    }
}

impl Section for AgentsSection {
    fn id(&self) -> &'static str {
        "agents"
    }

    fn summary(&self) -> Option<String> {
        self.agents.as_ref()?;
        let roster = self.roster.lock().unwrap_or_else(|e| e.into_inner());
        Some(state_line(&roster))
    }

    fn title(&self) -> &'static str {
        "Agents"
    }

    fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
        Box::pin(async move {
            let paused = self.pause.is_paused();
            let mut rows = vec![
                Row::new(Self::PAUSE, "Pause agents", Control::Toggle(paused))
                    .detail(if paused {
                        "Paused · leases held, publishes rejected"
                    } else {
                        "Running"
                    })
                    .keywords(&["safe mode", "resume", "suspend", "stop", "override"])
                    .caution(paused),
            ];
            if let Some(row) = self.answer_row() {
                rows.push(row);
            }
            let roster = self
                .roster
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            for entry in &roster {
                self.agent_rows(entry, &mut rows);
            }
            if self.connect {
                rows.push(
                    Row::new(
                        super::super::connect::OPEN,
                        "Connect an agent",
                        Control::Action { confirm: None },
                    )
                    .detail("Copy a prompt with a one-time code")
                    .keywords(&[
                        "pair",
                        "pairing",
                        "connect",
                        "prompt",
                        "code",
                        "add agent",
                    ]),
                );
            }
            rows
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if row == Self::ANSWER_WINDOW {
                return self.apply_answer(edit);
            }
            if let (Self::PAUSE, Edit::Set(paused)) = (row, &edit) {
                let paused = *paused;
                self.pause.set(paused).await;
                return Ok(());
            }
            let unsupported = || format!("{row} does not support {edit:?}");
            let (verb, agent) = Verb::parse(row).ok_or_else(unsupported)?;
            let agents = self.agents.as_ref().ok_or_else(unsupported)?;
            match (verb, edit.clone()) {
                (Verb::Manage, Edit::Set(open)) => {
                    *self.expanded() = open.then(|| agent.to_owned());
                }
                (Verb::Clear, Edit::Activate) => {
                    let Cleared { portals, holdings } = agents.clear(agent).await;
                    tracing::info!(
                        agent,
                        portals,
                        zones = holdings.zones,
                        widgets = holdings.widgets,
                        tiles = holdings.tiles,
                        "control panel: cleared agent"
                    );
                }
                (Verb::Unpair, Edit::Activate) => {
                    agents.unpair(agent).await?;
                    *self.expanded() = None;
                }
                (Verb::Grant(grant), Edit::Set(on)) => {
                    let result = agents.set_grant(agent, grant, on).await;
                    self.reload().await;
                    return result;
                }
                _ => return Err(unsupported()),
            }
            self.reload().await;
            Ok(())
        })
    }

    fn refresh(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.reload())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answer_window_saves_applies_and_resets_only_after_success() {
        use super::super::super::runtime::tests::fake;
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("c.toml");
        let mut control = fake(&config, Arc::default(), None);
        let service = tze_hud_protocol::session_server::HudSessionImpl::new(
            tze_hud_scene::SceneGraph::new(800.0, 600.0),
            "panel-test",
        );
        let pause = PauseControl::new(
            service.state,
            Arc::default(),
            Arc::default(),
            Default::default(),
            None,
            Default::default(),
        );
        let section = AgentsSection::new(pause, None, false, Some(control.clone()));
        let row = section.answer_row().unwrap();
        assert_eq!(row.label, "Questions wait for an answer");
        assert_eq!(
            row.control,
            Control::Choice {
                options: ["15 s", "30 s", "1 min", "2 min", "5 min"]
                    .map(str::to_owned)
                    .to_vec(),
                selected: 2,
            }
        );
        assert_eq!(
            row.keywords,
            ["question", "choice", "answer", "timeout", "wait"]
        );
        assert!(row.detail.unwrap().ends_with(" · Default"));
        assert!(!row.reset);
        for index in [0, 1, 2, 3, 4, 4] {
            section
                .apply(AgentsSection::ANSWER_WINDOW, Edit::Choose(index))
                .await
                .unwrap();
            let expected = ANSWER_CHOICES[index];
            assert_eq!(
                control.live.publication_defaults.answer_window_ms(),
                expected
            );
            let saved = std::fs::read_to_string(&config).unwrap();
            assert_eq!(
                tze_hud_config::runtime_section(&saved)
                    .unwrap()
                    .answer_window_ms,
                Some(expected)
            );
            let restarted = crate::launch::LiveSettings::new(60, false, Default::default())
                .with_answer_window(
                    tze_hud_config::runtime_section(&saved)
                        .unwrap()
                        .answer_window_ms
                        .unwrap(),
                );
            assert_eq!(restarted.publication_defaults.answer_window_ms(), expected);
            assert!(
                section
                    .answer_row()
                    .unwrap()
                    .detail
                    .unwrap()
                    .ends_with(" · Saved to config")
            );
        }
        for _ in 0..2 {
            section
                .apply(AgentsSection::ANSWER_WINDOW, Edit::Reset)
                .await
                .unwrap();
            assert_eq!(control.live.publication_defaults.answer_window_ms(), 60_000);
            assert!(
                !std::fs::read_to_string(&config)
                    .unwrap()
                    .contains("answer_window_ms")
            );
            assert_eq!(control.sources().answer_window_ms, Source::Default);
        }
        control
            .live
            .publication_defaults
            .set_answer_window_ms(12_345);
        let custom = section.answer_row().unwrap();
        assert!(
            matches!(custom.control, Control::Choice { options, selected } if options[selected] == "12345 ms")
        );
        control.sources.lock().unwrap().answer_window_ms = Source::Flag("test pin");
        assert!(
            section
                .apply(AgentsSection::ANSWER_WINDOW, Edit::Choose(0))
                .await
                .is_err()
        );
        assert!(
            section
                .apply(AgentsSection::ANSWER_WINDOW, Edit::Reset)
                .await
                .is_err()
        );
        control.sources.lock().unwrap().answer_window_ms = Source::Config;
        // The same failed writer used for no-file/read-only config behavior.
        control.config_path = None;
        control.write = Arc::new(|_| Err("no config file to save to".into()));
        let section = AgentsSection::new(section.pause, None, false, Some(control.clone()));
        for edit in [Edit::Choose(0), Edit::Reset] {
            assert_eq!(
                section
                    .apply(AgentsSection::ANSWER_WINDOW, edit)
                    .await
                    .unwrap_err(),
                "no config file to save to"
            );
            assert_eq!(control.live.publication_defaults.answer_window_ms(), 12_345);
            assert_eq!(control.sources().answer_window_ms, Source::Config);
        }
    }

    #[test]
    fn row_ids_round_trip_for_any_namespace() {
        for agent in ["claude", "agent.test", "a:b"] {
            for verb in Verb::all() {
                assert_eq!(Verb::parse(&verb.row(agent)), Some((verb, agent)));
            }
        }
        assert_eq!(Verb::parse("agents.pause"), None);
        assert_eq!(Verb::parse("agents.bogus:x"), None);
    }

    #[test]
    fn status_lines_name_connection_and_holdings() {
        let mut entry = AgentEntry {
            id: "a".into(),
            paired: true,
            allow: vec![],
            sessions: 1,
            portals: 1,
            holdings: tze_hud_scene::graph::AgentHoldings {
                zones: 2,
                widgets: 1,
                tiles: 0,
            },
        };
        assert_eq!(
            status_line(&entry),
            "Connected · 1 portal, 2 zones and 1 widget"
        );
        entry.sessions = 0;
        entry.portals = 0;
        entry.holdings.widgets = 0;
        assert_eq!(status_line(&entry), "Showing 2 zones");
        entry.holdings.zones = 0;
        entry.paired = false;
        assert_eq!(status_line(&entry), "Idle · holds nothing · not paired");
    }

    /// The header counts agents connected or showing something, and sums
    /// what they hold; an idle paired agent is not counted.
    #[test]
    fn state_line_counts_active_agents_and_what_they_hold() {
        let entry = |id: &str, sessions, portals, zones, widgets, tiles| AgentEntry {
            id: id.into(),
            paired: true,
            allow: vec![],
            sessions,
            portals,
            holdings: tze_hud_scene::graph::AgentHoldings {
                zones,
                widgets,
                tiles,
            },
        };
        assert_eq!(
            state_line(&[]),
            "0 agents · 0 portals · 0 zones · 0 widgets"
        );
        let roster = [
            entry("a", 1, 1, 2, 1, 0),
            entry("b", 0, 1, 3, 0, 0),
            entry("c", 1, 0, 0, 0, 0),
            entry("idle", 0, 0, 0, 0, 0),
        ];
        assert_eq!(
            state_line(&roster),
            "3 agents · 2 portals · 5 zones · 1 widget"
        );
    }
}
