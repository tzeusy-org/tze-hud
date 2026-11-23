//! MCP session scopes (`docs/api.md`, Identity and permissions).
//!
//! `initialize` opens a scope and the transport hands its id back as
//! `Mcp-Session-Id`; later calls carrying the id act within it. A scope owns
//! its content, holdings, portals and input under
//! [`tze_hud_scene::scope::scoped_namespace`]; budgets, the allowlist and human
//! overrides stay per agent. Calls without an id keep the agent's single
//! unscoped namespace. The table is bounded per agent; nothing expires by
//! time, so an idle session keeps its scope.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tze_hud_scene::scope::SessionLabel;
use tze_hud_scene::{Clock, SystemClock};

/// Live scopes one agent may hold; opening one more releases the
/// least-recently-called, preferring one with nothing on screen.
pub const MAX_SCOPES_PER_AGENT: usize = 64;
/// Longest session label, in bytes.
pub const MAX_LABEL_BYTES: usize = 32;

/// One live MCP session of an agent.
#[derive(Debug)]
pub struct Scope {
    /// The `Mcp-Session-Id` value.
    pub id: String,
    pub agent: String,
    /// The namespace this session owns content under.
    pub namespace: String,
    label: Mutex<Option<SessionLabel>>,
    last_call: AtomicU64,
    last_call_ms: AtomicU64,
    presence: Mutex<Option<ScopePresence>>,
    released: AtomicBool,
}

/// The last reported state of a live primary session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ScopePresence {
    pub state: &'static str,
    pub reason: Option<&'static str>,
    pub since_ms: u64,
}

impl Scope {
    /// The caption for this session's content, if it has one.
    pub fn label(&self) -> Option<String> {
        self.label
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|label| label.caption.clone())
    }

    pub(crate) fn session_label(&self) -> Option<SessionLabel> {
        self.label.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub(crate) fn set_label(&self, label: SessionLabel) {
        *self.label.lock().unwrap_or_else(|p| p.into_inner()) = Some(label);
    }

    /// True once DELETE or the table bound has released this session.
    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }
}

/// A live scope as `/admin/status` reports it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ScopeSummary {
    pub agent: String,
    pub label: Option<String>,
    pub namespace: String,
    pub last_call_ms: u64,
    pub presence: Option<ScopePresence>,
}

/// Every live scope, by session id.
#[derive(Debug)]
pub struct Scopes {
    live: Mutex<HashMap<String, Arc<Scope>>>,
    call_order: AtomicU64,
    clock: Arc<dyn Clock>,
}

impl Default for Scopes {
    fn default() -> Self {
        Self::with_clock(Arc::new(SystemClock::new()))
    }
}

impl Scopes {
    pub(crate) fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            live: Mutex::new(HashMap::new()),
            call_order: AtomicU64::new(0),
            clock,
        }
    }

    pub(crate) fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Scope>>> {
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn tick(&self) -> u64 {
        self.call_order.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The live scope `id` of `agent`, marked as just called. Another
    /// agent's id is unknown to this one.
    pub fn touch(&self, agent: &str, id: &str) -> Option<Arc<Scope>> {
        let scope = self.live().get(id).filter(|s| s.agent == agent).cloned()?;
        scope.last_call.fetch_max(self.tick(), Ordering::Relaxed);
        scope
            .last_call_ms
            .fetch_max(self.clock.now_millis(), Ordering::Relaxed);
        Some(scope)
    }

    pub(crate) fn report_presence(&self, scope: &Scope, params: &serde_json::Value) {
        let (state, reason) = match (
            params.get("state").and_then(serde_json::Value::as_str),
            params.get("reason"),
        ) {
            (Some("working"), None) => ("working", None),
            (Some("done"), None) => ("done", None),
            (Some("blocked"), Some(reason)) => match reason.as_str() {
                Some("question") => ("blocked", Some("question")),
                Some("permission") => ("blocked", Some("permission")),
                Some("input") => ("blocked", Some("input")),
                _ => return,
            },
            _ => return,
        };
        let mut current = scope.presence.lock().unwrap_or_else(|p| p.into_inner());
        if !scope.is_released()
            && !current.is_some_and(|presence| presence.state == state && presence.reason == reason)
        {
            *current = Some(ScopePresence {
                state,
                reason,
                since_ms: self.clock.now_millis(),
            });
        }
    }

    /// Open a scope for `agent` with a sanitized, de-duplicated `label`.
    /// Returns it and, when the agent is at its bound, the scope to release
    /// first: the least-recently-called among those `idle` says hold nothing,
    /// else the least-recently-called of all.
    pub fn open(
        &self,
        agent: &str,
        label: Option<&str>,
        idle: impl Fn(&Scope) -> bool,
    ) -> (Arc<Scope>, Option<Arc<Scope>>) {
        let mut live = self.live();
        let mine: Vec<&Arc<Scope>> = live.values().filter(|s| s.agent == agent).collect();
        let evict = (mine.len() >= MAX_SCOPES_PER_AGENT)
            .then(|| {
                let oldest = |pool: Vec<&Arc<Scope>>| {
                    pool.into_iter()
                        .min_by_key(|s| s.last_call.load(Ordering::Relaxed))
                        .cloned()
                };
                oldest(mine.iter().copied().filter(|s| idle(s)).collect())
                    .or_else(|| oldest(mine.clone()))
            })
            .flatten();
        if let Some(evicted) = &evict {
            evicted.released.store(true, Ordering::SeqCst);
            live.remove(&evicted.id);
        }
        let taken: Vec<String> = live
            .values()
            .filter(|s| s.agent == agent)
            .filter_map(|s| s.label())
            .collect();
        let label = label.and_then(sanitize_label).map(|mut label| {
            label.caption = unique_label(&label.caption, &taken);
            label
        });
        let id = new_session_id();
        let scope = Arc::new(Scope {
            namespace: tze_hud_scene::scope::scoped_namespace(agent, &id),
            id: id.clone(),
            agent: agent.to_owned(),
            label: Mutex::new(label),
            last_call: AtomicU64::new(self.tick()),
            last_call_ms: AtomicU64::new(self.clock.now_millis()),
            presence: Mutex::new(None),
            released: AtomicBool::new(false),
        });
        live.insert(id, Arc::clone(&scope));
        (scope, evict)
    }

    /// End scope `id` of `agent`; `None` if it is not live.
    pub fn close(&self, agent: &str, id: &str) -> Option<Arc<Scope>> {
        let mut live = self.live();
        live.get(id).filter(|s| s.agent == agent)?;
        let scope = live.remove(id)?;
        scope.released.store(true, Ordering::SeqCst);
        Some(scope)
    }

    /// The live scope owning `namespace`, if any.
    pub fn by_namespace(&self, namespace: &str) -> Option<Arc<Scope>> {
        let (_, id) = namespace.split_once(tze_hud_scene::scope::SCOPE_SEPARATOR)?;
        self.live()
            .get(id)
            .filter(|s| s.namespace == namespace)
            .cloned()
    }

    /// Every live scope, sorted by agent then label.
    pub fn list(&self) -> Vec<ScopeSummary> {
        let mut all: Vec<ScopeSummary> = self
            .live()
            .values()
            .map(|s| ScopeSummary {
                agent: s.agent.clone(),
                label: s.label(),
                namespace: s.namespace.clone(),
                last_call_ms: s.last_call_ms.load(Ordering::Relaxed),
                presence: *s.presence.lock().unwrap_or_else(|p| p.into_inner()),
            })
            .collect();
        all.sort_by(|a, b| {
            (&a.agent, &a.label, &a.namespace).cmp(&(&b.agent, &b.label, &b.namespace))
        });
        all
    }
}

/// Strip control characters and cut to [`MAX_LABEL_BYTES`] on a character
/// boundary; nothing left means no label.
pub(crate) fn sanitize_label(raw: &str) -> Option<SessionLabel> {
    let mut caption = String::new();
    let mut complete = true;
    for c in raw.chars().filter(|c| !c.is_control()) {
        if caption.len() + c.len_utf8() > MAX_LABEL_BYTES {
            complete = false;
            break;
        }
        caption.push(c);
    }
    let caption = caption.trim();
    (!caption.is_empty()).then(|| SessionLabel {
        caption: caption.to_owned(),
        window_token: complete.then(|| caption.to_owned()),
    })
}

/// `base`, or `base (n)` with the lowest free n >= 2 among `taken`.
fn unique_label(base: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base} ({n})"))
        .find(|candidate| !taken.contains(candidate))
        .expect("finitely many labels are taken")
}

/// 128 random bits as lowercase hex: unguessable, header-safe.
fn new_session_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS random source");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_sanitized_and_bounded() {
        assert_eq!(
            sanitize_label("tze-hud")
                .map(|label| label.caption)
                .as_deref(),
            Some("tze-hud")
        );
        assert_eq!(
            sanitize_label("a\u{7}b\nc")
                .map(|label| label.caption)
                .as_deref(),
            Some("abc")
        );
        assert!(sanitize_label("\u{1b}\t ").is_none());
        // 31 ASCII bytes then a 2-byte character would end at 33 bytes.
        let raw = format!("{}é{}", "x".repeat(31), "y".repeat(27));
        assert_eq!(raw.len(), 60);
        let truncated = sanitize_label(&raw).unwrap();
        assert_eq!(truncated.caption, "x".repeat(31));
        assert_eq!(truncated.window_token, None);
    }

    #[test]
    fn duplicate_live_labels_get_the_lowest_free_suffix() {
        let scopes = Scopes::default();
        let open = |label| scopes.open("claude", Some(label), |_| true).0;
        let first = open("tze-hud");
        let duplicate = open("tze-hud");
        assert_eq!(duplicate.label().as_deref(), Some("tze-hud (2)"));
        assert_eq!(
            duplicate.session_label().unwrap().window_token.as_deref(),
            Some("tze-hud")
        );
        let literal = open("tze-hud (2)");
        assert_eq!(
            literal.session_label().unwrap().window_token.as_deref(),
            Some("tze-hud (2)")
        );
        assert_eq!(open("tze-hud").label().as_deref(), Some("tze-hud (3)"));
        scopes.close("claude", &first.id).unwrap();
        assert_eq!(open("tze-hud").label().as_deref(), Some("tze-hud"));
        // Another agent's labels never collide.
        let other = scopes.open("codex", Some("tze-hud"), |_| true).0;
        assert_eq!(other.label().as_deref(), Some("tze-hud"));
    }

    #[test]
    fn ids_are_per_agent_and_closing_releases() {
        let scopes = Scopes::default();
        let (scope, evicted) = scopes.open("claude", None, |_| true);
        assert!(evicted.is_none());
        assert_eq!(scope.id.len(), 32);
        assert!(
            scopes.touch("codex", &scope.id).is_none(),
            "another agent's id"
        );
        assert!(scopes.touch("claude", &scope.id).is_some());
        assert!(scopes.by_namespace(&scope.namespace).is_some());
        assert!(scopes.close("codex", &scope.id).is_none());
        assert!(scopes.close("claude", &scope.id).unwrap().is_released());
        assert!(scopes.touch("claude", &scope.id).is_none());
        assert!(scopes.close("claude", &scope.id).is_none());
    }

    #[test]
    fn the_bound_releases_the_least_recently_called_idle_scope_first() {
        let scopes = Scopes::default();
        let all: Vec<_> = (0..MAX_SCOPES_PER_AGENT)
            .map(|_| scopes.open("claude", None, |_| true).0)
            .collect();
        // The oldest holds content; the next oldest is idle but called last.
        scopes.touch("claude", &all[1].id).unwrap();
        let busy = all[0].id.clone();
        let (_, evicted) = scopes.open("claude", None, |s| s.id != busy);
        let evicted = evicted.unwrap();
        assert_eq!(evicted.id, all[2].id);
        assert!(evicted.is_released());
        assert_eq!(scopes.list().len(), MAX_SCOPES_PER_AGENT);
        // With nothing idle, the least-recently-called goes.
        let (_, evicted) = scopes.open("claude", None, |_| false);
        assert_eq!(evicted.unwrap().id, busy);
    }

    #[test]
    fn presence_is_scoped_and_its_timestamp_is_the_last_transition() {
        let clock = tze_hud_scene::clock::TestClock::new(100);
        let scopes = Scopes::with_clock(Arc::new(clock.clone()));
        let (scope, _) = scopes.open("claude", None, |_| true);
        let (sibling_client, _) = scopes.open("claude", None, |_| true);
        let working = serde_json::json!({"state": "working"});
        for absent in [
            serde_json::json!({"state": "unknown"}),
            serde_json::json!({"state": "blocked"}),
            serde_json::json!({"state": "done", "reason": "permission"}),
        ] {
            scopes.report_presence(&scope, &absent);
        }
        assert!(scopes.list().iter().all(|row| row.presence.is_none()));
        clock.advance(25);
        scopes.touch("claude", &scope.id).unwrap();
        scopes.report_presence(&scope, &working);
        clock.advance(25);
        scopes.touch("claude", &scope.id).unwrap();
        scopes.report_presence(&scope, &working);
        let rows = scopes.list();
        let report = rows
            .iter()
            .find(|row| row.namespace == scope.namespace)
            .unwrap();
        assert_eq!(report.last_call_ms, 150);
        assert_eq!(report.presence.unwrap().since_ms, 125);
        assert!(
            rows.iter()
                .find(|row| row.namespace == sibling_client.namespace)
                .unwrap()
                .presence
                .is_none()
        );
        scopes.close("claude", &scope.id).unwrap();
        assert_eq!(scopes.list().len(), 1);
    }
}
