//! Bounded operator-only diagnostics. Sources are observed separately without
//! waiting, nesting locks, changing liveness, or copying published content.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tze_hud_scene::SceneGraph;
use tze_hud_scene::graph::holdings::Holding;
use tze_hud_scene::startup_diagnostics::StartupWarnings;

/// One runtime's actual startup producers. Widget results are already complete;
/// fonts finish only after the first configured compositor initialization.
#[derive(Clone, Debug)]
pub struct StartupDiagnostics(Arc<Mutex<StartupState>>);

#[derive(Debug)]
struct StartupState {
    warnings: StartupWarnings,
    fonts_complete: bool,
}

impl StartupDiagnostics {
    pub fn new(warnings: StartupWarnings) -> Self {
        Self(Arc::new(Mutex::new(StartupState {
            warnings,
            fonts_complete: false,
        })))
    }

    pub fn finish_fonts(&self, warnings: StartupWarnings) {
        let Ok(mut state) = self.0.lock() else {
            return;
        };
        if !state.fonts_complete {
            state.warnings.extend(warnings);
            state.fonts_complete = true;
        }
    }

    fn render(&self) -> Value {
        let Ok(state) = self.0.try_lock() else {
            return unavailable_warnings();
        };
        let mut items = state.warnings.items().to_vec();
        let mut omitted = state.warnings.omitted_events();
        let mut reasons = BTreeSet::new();
        if omitted != 0 {
            reasons.insert("cardinality");
        }
        loop {
            let value = json!({
                "state": if state.fonts_complete { "complete" } else { "pending" },
                "items": items,
                "truncated": omitted != 0,
                "omitted_events": omitted,
                "omission_reasons": reasons,
            });
            if serde_json::to_vec(&value)
                .expect("JSON values serialize")
                .len()
                <= 16_000
            {
                return value;
            }
            if items.pop().is_some() {
                omitted = omitted.saturating_add(1);
                reasons.insert("bytes");
            }
        }
    }
}

fn unavailable_warnings() -> Value {
    json!({"state":"unavailable","items":[],"truncated":false,"omitted_events":0,"omission_reasons":[]})
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    owner: String,
    surface: String,
    kind: &'static str,
    phase: &'static str,
    basis: &'static str,
    deadline: Option<u64>,
}

#[derive(Default)]
struct Holdings {
    // At most 64 owner counters and 256 rows, regardless of source cardinality.
    owners: BTreeMap<String, u64>,
    rows: BTreeMap<Row, (u64, u64)>, // represented records, remaining milliseconds
    omitted: u64,
    unknown_owner_omissions: bool,
    reasons: BTreeSet<&'static str>,
}

impl Holdings {
    fn omit(&mut self, owner: &str, count: u64, reason: &'static str) {
        self.omitted = self.omitted.saturating_add(count);
        if let Some(omitted) = self.owners.get_mut(owner) {
            *omitted = omitted.saturating_add(count);
        }
        self.reasons.insert(reason);
    }

    fn remove_last_row(&mut self, reason: &'static str) {
        if let Some((row, (count, _))) = self.rows.pop_last() {
            self.omit(&row.owner, count, reason);
        }
    }

    fn add(&mut self, holding: Holding<'_>) {
        // Never shorten an address into a different address, or copy a huge ID.
        if holding.owner.len() > 128 {
            self.unknown_owner_omissions = true;
            self.omit(holding.owner, 1, "oversize_id");
            return;
        }
        if !self.owners.contains_key(holding.owner) {
            if self.owners.len() == 64 {
                self.unknown_owner_omissions = true;
                let last = self.owners.last_key_value().expect("64 owners").0;
                if holding.owner >= last.as_str() {
                    self.omit(holding.owner, 1, "cardinality");
                    return;
                }
                let removed = last.clone();
                self.owners.remove(&removed);
                let mut records = 0u64;
                self.rows.retain(|row, (count, _)| {
                    if row.owner == removed {
                        records = records.saturating_add(*count);
                        false
                    } else {
                        true
                    }
                });
                self.omit(&removed, records, "cardinality");
            }
            self.owners.insert(holding.owner.to_owned(), 0);
        }
        if holding.id.len().saturating_add(holding.kind.len() + 1) > 256 {
            self.omit(holding.owner, 1, "oversize_id");
            return;
        }
        let row = Row {
            owner: holding.owner.to_owned(),
            surface: format!("{}:{}", holding.kind, holding.id),
            kind: holding.kind,
            phase: holding.phase,
            basis: holding.expiry_kind,
            deadline: holding.deadline_wall_us,
        };
        let remaining = holding.deadline_wall_us.map_or(0, |at| {
            at.saturating_sub(holding.now_wall_us).div_ceil(1000)
        });
        let entry = self.rows.entry(row).or_insert((0, remaining));
        entry.0 = entry.0.saturating_add(1);
        if self.rows.len() > 256 {
            self.remove_last_row("cardinality");
        }
    }

    fn render(&mut self, scene: &'static str, portals: &'static str) -> Value {
        let available = scene == "available" || portals == "available";
        let sources_complete =
            scene == "available" && matches!(portals, "available" | "not_configured");
        let unknown_omissions = !sources_complete;
        if !sources_complete {
            self.reasons
                .insert(if scene == "busy" || portals == "busy" {
                    "busy"
                } else {
                    "unavailable"
                });
        }
        loop {
            let mut agents = Vec::new();
            for (owner, omitted) in &self.owners {
                let surfaces: Vec<_> = self.rows.iter().filter(|(row, _)| &row.owner == owner).map(|(row, (count, remaining))| {
                    json!({"surface":row.surface,"kind":row.kind,"phase":row.phase,
                        "expires_in_ms":row.deadline.map(|_| *remaining),"expiry_kind":row.basis,"count":count})
                }).collect();
                if !surfaces.is_empty() {
                    agents.push(json!({"id":owner,"surfaces":surfaces,"omitted_records":omitted}));
                }
            }
            let value = json!({
                "state": if sources_complete && self.omitted == 0 { "complete" } else if available { "partial" } else { "unavailable" },
                "sources":{"scene":scene,"portals":portals},"agents":agents,
                // Owners lost before bounded admission cannot be counted
                // distinctly without an unbounded set. Row/byte loss within
                // our admitted owner set has an exact bounded count.
                "omitted_agents": if unknown_omissions || self.unknown_owner_omissions { None } else {
                    Some(self.owners.keys().filter(|owner| !self.rows.keys().any(|row| &row.owner == *owner)).count() as u64)
                },
                "omitted_records": if unknown_omissions { None } else { Some(self.omitted) },
                "omission_reasons":self.reasons,
            });
            if serde_json::to_vec(&value)
                .expect("JSON values serialize")
                .len()
                <= 49_152
            {
                return value;
            }
            self.remove_last_row("bytes");
        }
    }
}

pub(crate) fn render(
    scene: Option<&Arc<tokio::sync::Mutex<SceneGraph>>>,
    portals: Option<&tze_hud_mcp::PortalHandle>,
    startup: Option<&StartupDiagnostics>,
) -> (Value, Value) {
    let mut holdings = Holdings::default();
    let scene_state = match scene {
        Some(scene) => match scene.try_lock() {
            Ok(scene) => {
                scene.visit_holdings(|row| holdings.add(row));
                "available"
            }
            Err(_) => "busy",
        },
        None => "unavailable",
    };
    // The scene guard above is gone before acquiring the independent hub.
    let portal_state = match portals {
        Some(portals) => match portals.hub.try_lock() {
            Ok(hub) => {
                let now = portals.clock.now_us();
                hub.visit_holdings(|key, deadline, basis| {
                    holdings.add(Holding {
                        owner: &key.agent,
                        id: &key.id,
                        kind: "portal",
                        phase: if deadline.is_some_and(|at| at <= now) {
                            "cleanup_due"
                        } else {
                            "active"
                        },
                        expiry_kind: basis,
                        deadline_wall_us: deadline,
                        now_wall_us: now,
                    })
                });
                "available"
            }
            Err(std::sync::TryLockError::WouldBlock) => "busy",
            Err(std::sync::TryLockError::Poisoned(_)) => "unavailable",
        },
        None => "not_configured",
    };
    (
        holdings.render(scene_state, portal_state),
        startup.map_or_else(unavailable_warnings, StartupDiagnostics::render),
    )
}
