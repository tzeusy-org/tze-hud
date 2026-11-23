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
    released: AtomicBool,
}

impl Scope {
    /// The caption for this session's content, if it has one.
    pub fn label(&self) -> Option<String> {
        self.session_label().map(|label| label.caption)
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
}

/// Every live scope, by session id.
#[derive(Debug, Default)]
pub struct Scopes {
    live: Mutex<HashMap<String, Arc<Scope>>>,
    clock: AtomicU64,
}

impl Scopes {
    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Scope>>> {
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The live scope `id` of `agent`, marked as just called. Another
    /// agent's id is unknown to this one.
    pub fn touch(&self, agent: &str, id: &str) -> Option<Arc<Scope>> {
        let scope = self.live().get(id).filter(|s| s.agent == agent).cloned()?;
        scope.last_call.store(self.tick(), Ordering::Relaxed);
        Some(scope)
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
        assert_eq!(open("tze-hud").label().as_deref(), Some("tze-hud (2)"));
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
}
