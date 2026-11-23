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
    /// The primary session id, for a linked sibling.
    pub parent: Option<String>,
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
    pub parent_namespace: Option<String>,
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
        let scope = self.find(agent, id)?;
        self.record_call(&scope);
        Some(scope)
    }

    pub(crate) fn find(&self, agent: &str, id: &str) -> Option<Arc<Scope>> {
        self.live().get(id).filter(|s| s.agent == agent).cloned()
    }

    pub(crate) fn record_call(&self, scope: &Scope) {
        scope.last_call.fetch_max(self.tick(), Ordering::Relaxed);
        scope
            .last_call_ms
            .fetch_max(self.clock.now_millis(), Ordering::Relaxed);
    }

    pub(crate) fn report_presence(&self, scope: &Scope, params: &serde_json::Value) {
        if scope.parent.is_some() {
            return;
        }
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

    /// Open an ordinary scope with a sanitized, de-duplicated label.
    pub fn open(
        &self,
        agent: &str,
        label: Option<&str>,
        idle: impl Fn(&Scope) -> bool,
    ) -> (Arc<Scope>, Vec<Arc<Scope>>) {
        self.open_with_parent(agent, label, None, idle)
            .expect("opening an ordinary scope cannot reject a parent")
    }

    /// Open a scope and return any scopes released to make room.
    /// Linking validates the parent and its single sibling slot under the
    /// same lock as the insertion. Invalid links leave the table unchanged.
    pub(crate) fn open_with_parent(
        &self,
        agent: &str,
        label: Option<&str>,
        parent: Option<&str>,
        idle: impl Fn(&Scope) -> bool,
    ) -> Option<(Arc<Scope>, Vec<Arc<Scope>>)> {
        let mut live = self.live();
        let parent = match parent {
            Some(id) => {
                let primary = live
                    .get(id)
                    .filter(|s| s.agent == agent && s.parent.is_none())?;
                if live.values().any(|s| s.parent.as_deref() == Some(id)) {
                    return None;
                }
                Some(Arc::clone(primary))
            }
            None => None,
        };
        let mine: Vec<&Arc<Scope>> = live.values().filter(|s| s.agent == agent).collect();
        let evicted = if mine.len() >= MAX_SCOPES_PER_AGENT {
            // A sibling admission cannot release the parent it just validated.
            let evict = mine
                .into_iter()
                .filter(|s| parent.as_ref().is_none_or(|p| s.id != p.id))
                .min_by_key(|s| (!idle(s), s.last_call.load(Ordering::Relaxed)))
                .cloned()
                .expect("the table bound leaves other eviction candidates");
            Self::remove(&mut live, &evict.id)
        } else {
            Vec::new()
        };
        let label = match &parent {
            Some(primary) => primary.session_label(),
            None => label.and_then(sanitize_label).map(|mut label| {
                let taken: Vec<String> = live
                    .values()
                    .filter(|s| s.agent == agent && s.parent.is_none())
                    .filter_map(|s| s.label())
                    .collect();
                label.caption = unique_label(&label.caption, &taken);
                label
            }),
        };
        let id = new_session_id();
        let scope = Arc::new(Scope {
            namespace: tze_hud_scene::scope::scoped_namespace(agent, &id),
            id: id.clone(),
            agent: agent.to_owned(),
            parent: parent.map(|p| p.id.clone()),
            label: Mutex::new(label),
            last_call: AtomicU64::new(self.tick()),
            last_call_ms: AtomicU64::new(self.clock.now_millis()),
            presence: Mutex::new(None),
            released: AtomicBool::new(false),
        });
        live.insert(id, Arc::clone(&scope));
        Some((scope, evicted))
    }

    /// End scope `id` of `agent` and its sibling, if any.
    pub fn close(&self, agent: &str, id: &str) -> Vec<Arc<Scope>> {
        let mut live = self.live();
        if live.get(id).is_some_and(|s| s.agent == agent) {
            Self::remove(&mut live, id)
        } else {
            Vec::new()
        }
    }

    fn remove(live: &mut HashMap<String, Arc<Scope>>, id: &str) -> Vec<Arc<Scope>> {
        let mut released = live.remove(id).into_iter().collect::<Vec<_>>();
        let sibling = live
            .values()
            .find(|s| s.parent.as_deref() == Some(id))
            .map(|s| s.id.clone());
        if let Some(sibling) = sibling.and_then(|s| live.remove(&s)) {
            released.push(sibling);
        }
        for scope in &released {
            scope.released.store(true, Ordering::SeqCst);
        }
        released
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
                parent_namespace: s
                    .parent
                    .as_ref()
                    .map(|parent| tze_hud_scene::scope::scoped_namespace(&s.agent, parent)),
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
        assert_eq!(scopes.close("claude", &first.id).len(), 1);
        assert_eq!(open("tze-hud").label().as_deref(), Some("tze-hud"));
        // Another agent's labels never collide.
        let other = scopes.open("codex", Some("tze-hud"), |_| true).0;
        assert_eq!(other.label().as_deref(), Some("tze-hud"));
    }

    #[test]
    fn ids_are_per_agent_and_closing_releases() {
        let scopes = Scopes::default();
        let (scope, evicted) = scopes.open("claude", None, |_| true);
        assert!(evicted.is_empty());
        assert_eq!(scope.id.len(), 32);
        assert!(
            scopes.touch("codex", &scope.id).is_none(),
            "another agent's id"
        );
        assert!(scopes.touch("claude", &scope.id).is_some());
        assert!(scopes.by_namespace(&scope.namespace).is_some());
        assert!(scopes.close("codex", &scope.id).is_empty());
        assert!(scopes.close("claude", &scope.id)[0].is_released());
        assert!(scopes.touch("claude", &scope.id).is_none());
        assert!(scopes.close("claude", &scope.id).is_empty());
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
        let [evicted] = evicted.as_slice() else {
            panic!("expected one released scope");
        };
        assert_eq!(evicted.id, all[2].id);
        assert!(evicted.is_released());
        assert_eq!(scopes.list().len(), MAX_SCOPES_PER_AGENT);
        // With nothing idle, the least-recently-called goes.
        let (_, evicted) = scopes.open("claude", None, |_| false);
        assert_eq!(evicted[0].id, busy);
    }

    #[test]
    fn presence_is_scoped_and_its_timestamp_is_the_last_transition() {
        let clock = tze_hud_scene::clock::TestClock::new(100);
        let scopes = Scopes::with_clock(Arc::new(clock.clone()));
        let (scope, _) = scopes.open("claude", None, |_| true);
        let (sibling_client, _) = scopes
            .open_with_parent("claude", None, Some(&scope.id), |_| true)
            .unwrap();
        let working = serde_json::json!({"state": "working"});
        for absent in [
            serde_json::json!({"state": "unknown"}),
            serde_json::json!({"state": "blocked"}),
            serde_json::json!({"state": "done", "reason": "permission"}),
        ] {
            scopes.report_presence(&scope, &absent);
        }
        scopes.report_presence(&sibling_client, &working);
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
        assert_eq!(scopes.close("claude", &scope.id).len(), 2);
        assert!(scopes.list().is_empty());
    }
}
