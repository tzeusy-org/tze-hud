//! Session scopes: one agent's ownership split per MCP session.
//!
//! A scoped MCP session owns its content, holdings, portals and input under a
//! namespace `agent␟session` (unit separator), so every per-publisher rule
//! (key slots, the action FIFO, held/busy) isolates sessions with no new
//! field. Unscoped callers keep the bare agent id. Agent-wide rules (human
//! Clear, stack publisher caps, the control panel roster) read the owning
//! agent through [`owner_agent`].

/// Runtime-only caption and its original title token. A deduplicated
/// caption can have a suffix that the window's title never had.
#[derive(Clone, Debug)]
pub struct SessionLabel {
    pub caption: String,
    pub window_token: Option<String>,
}

/// Joins agent and session; no agent id or session id contains it.
pub const SCOPE_SEPARATOR: char = '\u{1f}';

/// The namespace a session of `agent` publishes under.
pub fn scoped_namespace(agent: &str, session: &str) -> String {
    format!("{agent}{SCOPE_SEPARATOR}{session}")
}

/// The agent that owns `namespace`, scoped or not.
pub fn owner_agent(namespace: &str) -> &str {
    namespace
        .split_once(SCOPE_SEPARATOR)
        .map_or(namespace, |(agent, _)| agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_namespaces_resolve_to_their_agent() {
        let scoped = scoped_namespace("claude", "ab12");
        assert_ne!(scoped, "claude");
        assert_eq!(owner_agent(&scoped), "claude");
        assert_eq!(owner_agent("claude"), "claude");
    }
}
