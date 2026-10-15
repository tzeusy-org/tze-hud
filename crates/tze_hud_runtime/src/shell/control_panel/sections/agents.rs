//! Agents: pause and resume every agent, and one row per agent with its
//! connection and holdings. Opening an agent's row shows Clear, Unpair and
//! its allow-list grants (CP-8).

use std::sync::Mutex;

use tze_hud_compositor::PanelTone;
use tze_hud_config::Grant;

use super::super::agents::{AgentControl, AgentEntry, Cleared};
use super::super::pause::PauseControl;
use super::super::row::{BoxFuture, Control, Edit, Row, Section};

pub(crate) struct AgentsSection {
    pause: PauseControl,
    /// `None` when the HUD has no agent services (no network runtime).
    agents: Option<AgentControl>,
    /// The agent whose actions are showing.
    expanded: Mutex<Option<String>>,
    /// The roster as last read: reading it takes the session, scene and
    /// portal locks, so rows read this and `refresh` renews it.
    roster: Mutex<Vec<AgentEntry>>,
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
            Verb::Grant(Grant::Tiles) => "grant.tiles",
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

/// "a portal, 2 zones and 1 widget", or `None` for nothing.
fn holdings_phrase(entry: &AgentEntry) -> Option<String> {
    let h = entry.holdings;
    let parts: Vec<String> = [
        count(entry.portals, "portal", "portals"),
        count(h.zones, "zone", "zones"),
        count(h.widgets, "widget", "widgets"),
        count(h.tiles, "tile", "tiles"),
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
        Grant::Tiles => "Tiles",
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
        Grant::Tiles => "Claim and draw its own tiles",
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
    pub(crate) const PAUSE: &'static str = "agents.pause";

    pub(crate) fn new(pause: PauseControl, agents: Option<AgentControl>) -> Self {
        Self {
            pause,
            agents,
            expanded: Mutex::new(None),
            roster: Mutex::new(Vec::new()),
        }
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

impl Section for AgentsSection {
    fn id(&self) -> &'static str {
        "agents"
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
            let roster = self
                .roster
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            for entry in &roster {
                self.agent_rows(entry, &mut rows);
            }
            rows
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let (Self::PAUSE, Edit::Set(paused)) = (row, edit) {
                self.pause.set(paused).await;
                return Ok(());
            }
            let unsupported = || format!("{row} does not support {edit:?}");
            let (verb, agent) = Verb::parse(row).ok_or_else(unsupported)?;
            let agents = self.agents.as_ref().ok_or_else(unsupported)?;
            match (verb, edit) {
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
}
