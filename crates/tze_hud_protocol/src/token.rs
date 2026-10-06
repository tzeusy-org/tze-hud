//! Session resume token management (RFC 0005 §6.1–6.6).
//!
//! # Overview
//!
//! When a session stream closes (gracefully or ungracefully), the runtime stores
//! a [`ResumeEntry`] keyed by the session's resume token bytes.  The entry holds:
//!
//! - the agent's namespace/`agent_id` (for binding validation),
//! - the capabilities that were granted in the original session,
//! - the active subscription set at disconnect time,
//! - the IDs of leases that became orphaned at disconnect,
//! - the wall-clock time at which the grace period expires.
//!
//! Tokens are:
//! - **opaque** — random 16-byte (UUIDv7) values,
//! - **single-use** — consumed on first successful resume attempt,
//! - **bound** to `agent_id` — cannot be used by a different agent,
//! - **in-memory only** — not persisted across process restarts (RFC 0005 §6.6).
//!
//! [`TokenStore`] is intentionally `Send + Sync` so it can live inside a
//! `tokio::sync::Mutex<SharedState>`.

use std::collections::HashMap;
use tze_hud_scene::SceneId;

/// Reconnect grace period in milliseconds: the scene's orphan grace period,
/// so a resume token and the leases it restores expire together.
pub const DEFAULT_GRACE_PERIOD_MS: u64 = tze_hud_scene::lease::ORPHAN_GRACE_PERIOD_MS;

/// A pending resume entry held for a disconnected agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeEntry {
    /// The `agent_id` that owned the original session.
    /// Used to prevent a different agent from stealing a resume token.
    pub agent_id: String,
    /// Capabilities that were granted for the original session.
    /// Restored verbatim on successful resume (v1: same grants, no re-negotiation).
    pub capabilities: Vec<String>,
    /// Subscription set active at disconnect time.
    /// Restored on successful resume; agents MUST use the confirmed set from
    /// [`SessionResumeResult`] rather than assuming their pre-disconnect set
    /// is intact.
    pub subscriptions: Vec<String>,
    /// Lease IDs that were held by the agent at disconnect.
    /// The runtime may use these to reclaim/restore orphaned leases.
    pub orphaned_lease_ids: Vec<SceneId>,
    /// Wall-clock expiry time in milliseconds (since UNIX epoch).
    /// After this instant the token is invalid regardless of other fields.
    pub expires_at_ms: u64,
}

impl ResumeEntry {
    /// Returns `true` if the entry is still within the grace period.
    pub fn is_valid(&self, now_ms: u64) -> bool {
        now_ms < self.expires_at_ms
    }
}

/// In-memory store for pending session resume tokens (RFC 0005 §6.1).
///
/// All tokens are cleared when the runtime process restarts because this
/// store is never persisted to disk.
#[derive(Debug, Default)]
pub struct TokenStore {
    /// Map from raw token bytes to the associated resume entry.
    entries: HashMap<Vec<u8>, ResumeEntry>,
}

impl TokenStore {
    /// Create an empty token store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a new resume token for a disconnected session.
    ///
    /// `token` must be cryptographically random and unique (e.g. UUIDv7 bytes).
    /// `grace_period_ms` is the duration from `now_ms` until the token expires.
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &mut self,
        token: Vec<u8>,
        agent_id: String,
        capabilities: Vec<String>,
        subscriptions: Vec<String>,
        orphaned_lease_ids: Vec<SceneId>,
        grace_period_ms: u64,
        now_ms: u64,
    ) {
        let entry = ResumeEntry {
            agent_id,
            capabilities,
            subscriptions,
            orphaned_lease_ids,
            expires_at_ms: now_ms.saturating_add(grace_period_ms),
        };
        self.entries.insert(token, entry);
    }

    /// Attempt to consume a resume token.
    ///
    /// Returns `Ok(ResumeEntry)` if the token is found, not expired, and the
    /// `agent_id` matches.  The token is **consumed** (removed) on success —
    /// a second attempt with the same token will fail.
    ///
    /// Returns `Err(ResumeError)` if the token is missing, expired, or belongs
    /// to a different agent.
    pub fn consume(
        &mut self,
        token: &[u8],
        agent_id: &str,
        now_ms: u64,
    ) -> Result<ResumeEntry, ResumeError> {
        // Peek at the entry before removing it, so that a wrong-agent attempt
        // does not destroy a valid token (DoS prevention).
        let verdict = match self.entries.get(token) {
            None => Err(ResumeError::TokenNotFound),
            Some(entry) => {
                if !entry.is_valid(now_ms) {
                    // Expired — always safe to drop; the token is unusable.
                    Err(ResumeError::GraceExpired)
                } else if entry.agent_id != agent_id {
                    // Token exists but belongs to a different agent — leave it
                    // in place so the legitimate owner can still resume.
                    // Treat as not-found to avoid leaking information about other sessions.
                    Err(ResumeError::TokenNotFound)
                } else {
                    // All checks passed — safe to consume.
                    Ok(())
                }
            }
        };

        match verdict {
            Err(ResumeError::GraceExpired) => {
                self.entries.remove(token);
                Err(ResumeError::GraceExpired)
            }
            Err(e) => Err(e),
            Ok(()) => {
                // SAFETY: entry was found and validated above; remove returns Some.
                let entry = self
                    .entries
                    .remove(token)
                    .expect("entry must exist after validation");
                Ok(entry)
            }
        }
    }

    /// Remove expired entries in token-store fixtures.
    #[cfg(test)]
    fn evict_expired(&mut self, now_ms: u64) {
        self.entries.retain(|_, e| e.is_valid(now_ms));
    }

    /// Inspect the pending entries in expiry fixtures.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Reasons why a resume token validation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeError {
    /// Token not present in the store (never issued, already consumed, or process restarted).
    TokenNotFound,
    /// Token was found but the grace period has elapsed.
    GraceExpired,
}

impl ResumeError {
    /// The `SessionError.code` string sent to the client (RFC 0005 §6.2, §6.5).
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::TokenNotFound => "SESSION_GRACE_EXPIRED",
            Self::GraceExpired => "SESSION_GRACE_EXPIRED",
        }
    }

    /// Human-readable message for the client.
    pub fn message(&self) -> &'static str {
        match self {
            Self::TokenNotFound => {
                "Resume token not found; grace period may have expired or runtime restarted"
            }
            Self::GraceExpired => "Resume token expired; reconnect grace period has elapsed",
        }
    }

    /// Hint directing the client to perform a full SessionInit handshake.
    pub fn hint(&self) -> &'static str {
        "Send a fresh SessionInit to start a new session"
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_token() -> Vec<u8> {
        uuid::Uuid::now_v7().as_bytes().to_vec()
    }

    fn insert_entry(
        store: &mut TokenStore,
        token: &[u8],
        agent_id: &str,
        grace_ms: u64,
        now_ms: u64,
    ) {
        store.insert(
            token.to_vec(),
            agent_id.to_string(),
            vec!["create_tile".to_string()],
            vec!["SCENE_TOPOLOGY".to_string()],
            vec![],
            grace_ms,
            now_ms,
        );
    }

    /// Token consumed successfully within the grace period.
    #[test]
    fn test_consume_valid_token() {
        let mut store = TokenStore::new();
        let token = make_token();
        let now = 1_000_000u64;
        insert_entry(&mut store, &token, "agent-a", 30_000, now);

        let result = store.consume(&token, "agent-a", now + 5_000);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let entry = result.unwrap();
        assert_eq!(entry.agent_id, "agent-a");
        assert!(entry.capabilities.contains(&"create_tile".to_string()));
    }

    /// Token is single-use: second consume attempt fails.
    #[test]
    fn test_token_single_use() {
        let mut store = TokenStore::new();
        let token = make_token();
        let now = 1_000_000u64;
        insert_entry(&mut store, &token, "agent-a", 30_000, now);

        let first = store.consume(&token, "agent-a", now + 1_000);
        assert!(first.is_ok(), "first consume should succeed");

        let second = store.consume(&token, "agent-a", now + 2_000);
        assert_eq!(
            second,
            Err(ResumeError::TokenNotFound),
            "second consume should fail"
        );
    }

    /// Token rejected after grace period expires.
    #[test]
    fn test_token_expired_grace_period() {
        let mut store = TokenStore::new();
        let token = make_token();
        let now = 1_000_000u64;
        // Grace period of 30 seconds
        insert_entry(&mut store, &token, "agent-a", 30_000, now);

        // Attempt 31 seconds later — expired
        let result = store.consume(&token, "agent-a", now + 31_000);
        assert_eq!(result, Err(ResumeError::GraceExpired));
        assert_eq!(result.unwrap_err().error_code(), "SESSION_GRACE_EXPIRED");
    }

    /// Token not found returns SESSION_GRACE_EXPIRED (covers runtime restart scenario).
    #[test]
    fn test_token_not_found_returns_grace_expired_code() {
        let mut store = TokenStore::new();
        let bogus_token = make_token();

        let result = store.consume(&bogus_token, "agent-a", 1_000_000);
        assert_eq!(result, Err(ResumeError::TokenNotFound));
        assert_eq!(result.unwrap_err().error_code(), "SESSION_GRACE_EXPIRED");
    }

    /// Token belonging to a different agent is rejected.
    #[test]
    fn test_token_wrong_agent_rejected() {
        let mut store = TokenStore::new();
        let token = make_token();
        let now = 1_000_000u64;
        insert_entry(&mut store, &token, "agent-a", 30_000, now);

        let result = store.consume(&token, "agent-b", now + 5_000);
        // Treated as TokenNotFound to avoid leaking info
        assert_eq!(result, Err(ResumeError::TokenNotFound));
    }

    /// Wrong-agent attempt must NOT destroy the token (DoS prevention).
    ///
    /// A malicious caller who knows a token but not its agent_id must not be
    /// able to invalidate it for the legitimate owner.
    #[test]
    fn test_wrong_agent_does_not_consume_token() {
        let mut store = TokenStore::new();
        let token = make_token();
        let now = 1_000_000u64;
        insert_entry(&mut store, &token, "agent-a", 30_000, now);

        // Attempt with wrong agent_id — must fail without removing the entry.
        let bad = store.consume(&token, "evil-agent", now + 1_000);
        assert_eq!(bad, Err(ResumeError::TokenNotFound));

        // The legitimate owner must still be able to consume the token.
        let good = store.consume(&token, "agent-a", now + 2_000);
        assert!(
            good.is_ok(),
            "legitimate owner should still be able to resume: {good:?}"
        );
    }

    /// Expired entries are removed by evict_expired.
    #[test]
    fn test_evict_expired() {
        let mut store = TokenStore::new();
        let now = 1_000_000u64;

        let token_a = make_token();
        let token_b = make_token();

        // token_a: 5s grace — will expire
        insert_entry(&mut store, &token_a, "agent-a", 5_000, now);
        // token_b: 60s grace — still valid
        insert_entry(&mut store, &token_b, "agent-b", 60_000, now);

        assert_eq!(store.len(), 2);

        // Advance time by 10 seconds
        store.evict_expired(now + 10_000);

        assert_eq!(
            store.len(),
            1,
            "expired entry for agent-a should be evicted"
        );

        let result = store.consume(&token_b, "agent-b", now + 10_000);
        assert!(
            result.is_ok(),
            "valid token should still be consumable after eviction"
        );
    }

    /// Exactly-at-expiry is treated as expired (strict less-than).
    #[test]
    fn test_token_at_exact_expiry_is_expired() {
        let mut store = TokenStore::new();
        let token = make_token();
        let now = 1_000_000u64;
        insert_entry(&mut store, &token, "agent-a", 30_000, now);

        // now + 30_000 == expires_at_ms → NOT valid (strict <)
        let result = store.consume(&token, "agent-a", now + 30_000);
        assert_eq!(result, Err(ResumeError::GraceExpired));
    }

    /// Token consumed 1ms before expiry succeeds.
    #[test]
    fn test_token_just_before_expiry_succeeds() {
        let mut store = TokenStore::new();
        let token = make_token();
        let now = 1_000_000u64;
        insert_entry(&mut store, &token, "agent-a", 30_000, now);

        let result = store.consume(&token, "agent-a", now + 29_999);
        assert!(result.is_ok());
    }

    /// Orphaned lease IDs are preserved in the resume entry.
    #[test]
    fn test_orphaned_leases_preserved() {
        let mut store = TokenStore::new();
        let token = make_token();
        let now = 1_000_000u64;
        let lease_id = SceneId::new();

        store.insert(
            token.clone(),
            "agent-a".to_string(),
            vec![],
            vec![],
            vec![lease_id],
            30_000,
            now,
        );

        let entry = store.consume(&token, "agent-a", now + 1_000).unwrap();
        assert_eq!(entry.orphaned_lease_ids.len(), 1);
        assert_eq!(entry.orphaned_lease_ids[0], lease_id);
    }
}
