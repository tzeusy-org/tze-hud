//! # Agent Event Emission Constants and Rate Limiter
//!
//! Shared constants and rate-limiter type for agent scene event emission,
//! used by both the protocol layer (`tze_hud_protocol`) and the runtime layer
//! (`tze_hud_runtime`).
//!
//! Implements scene-events/spec.md §5.3–§5.4:
//! - 4 KB maximum payload size (spec line 122).
//! - Sliding-window rate limit: default 10 events/second per agent session
//!   (spec lines 126-133).
//!
//! ## Why here?
//!
//! Both `tze_hud_protocol::session_server` and `tze_hud_runtime::agent_events`
//! need the emission definitions. `tze_hud_scene` is the common zero-dep
//! dependency of all of these crates, so it is the natural home for shared
//! numeric contracts.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

// ─── Emission constants ───────────────────────────────────────────────────────

/// Maximum allowed payload size in bytes (spec line 122: "limited to 4KB maximum").
pub const MAX_PAYLOAD_BYTES: usize = 4096;

/// Default rate limit per agent session (spec line 127: "default: 10 events/second").
pub const DEFAULT_MAX_EVENTS_PER_SECOND: u32 = 10;

// ─── Rate limiter ─────────────────────────────────────────────────────────────

/// One-second sliding window per the spec.
const WINDOW: Duration = Duration::from_secs(1);

/// Sliding-window rate limiter for a single agent session.
///
/// Implements scene-events/spec.md §5.4, Requirement: Agent Event Rate Limiting
/// (lines 126-133):
///
/// > Agent event emission SHALL be rate limited to a configurable maximum
/// > (default: 10 events/second per agent session). Rate limiting SHALL use a
/// > sliding window (1-second window, max count). Events exceeding the rate
/// > limit SHALL be rejected with RuntimeError code AGENT_EVENT_RATE_EXCEEDED.
///
/// ## Algorithm
///
/// The sliding window tracks the `Instant` of each accepted emission in a
/// `VecDeque`.  On each call to `check_and_record`:
///
/// 1. Expired entries (older than 1 second from `now`) are dropped from the
///    front of the deque.
/// 2. If the remaining count is ≥ the limit, the call is rejected.
/// 3. Otherwise the current `now` is pushed to the back and `Ok(())` is
///    returned.
///
/// This provides O(n) amortised per-check complexity where n is the window
/// count (bounded by `max_per_second`).
#[derive(Debug)]
pub struct AgentEventRateLimiter {
    /// Timestamps of accepted events within the current window.
    timestamps: VecDeque<Instant>,
    /// Maximum events accepted within a 1-second sliding window.
    max_per_second: u32,
}

impl AgentEventRateLimiter {
    /// Create a new limiter with the default rate (10 events/second).
    pub fn new() -> Self {
        Self::with_limit(DEFAULT_MAX_EVENTS_PER_SECOND)
    }

    /// Create a new limiter with a custom rate limit.
    pub fn with_limit(max_per_second: u32) -> Self {
        Self {
            timestamps: VecDeque::new(),
            max_per_second,
        }
    }

    /// Check whether a new event is within the rate limit and, if so, record it.
    ///
    /// - Returns `Ok(())` if the event is accepted (and records `now`).
    /// - Returns `Err(())` if the event would exceed the limit (does **not**
    ///   record the timestamp; the event is dropped).
    ///
    /// Callers should pass `Instant::now()` for `now` in production code.
    /// For deterministic testing, pass a synthetic `Instant` value.
    #[allow(clippy::result_unit_err)] // unit error is intentional: rate-limit is binary (ok/throttled)
    pub fn check_and_record(&mut self, now: Instant) -> Result<(), ()> {
        // Prune events that have left the 1-second window.
        while let Some(&front) = self.timestamps.front() {
            if now.duration_since(front) >= WINDOW {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }

        // If we are already at or over the limit, reject.
        if self.timestamps.len() as u32 >= self.max_per_second {
            return Err(());
        }

        // Accept the event.
        self.timestamps.push_back(now);
        Ok(())
    }

    /// Current count of events in the active window (informational).
    pub fn current_count(&self, now: Instant) -> usize {
        self.timestamps
            .iter()
            .filter(|&&t| now.duration_since(t) < WINDOW)
            .count()
    }

    /// Configured maximum events per second.
    pub fn max_per_second(&self) -> u32 {
        self.max_per_second
    }
}

impl Default for AgentEventRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Helper: create a synthetic instant offset by `millis` from a base.
    fn ms_after(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    // ── Basic acceptance ──────────────────────────────────────────────────────

    /// First 10 events in a 1-second window are accepted (default limit = 10).
    #[test]
    fn ten_events_accepted_within_window() {
        let base = Instant::now();
        let mut limiter = AgentEventRateLimiter::new();

        for i in 0..10 {
            let t = ms_after(base, i * 50); // one every 50ms
            assert!(
                limiter.check_and_record(t).is_ok(),
                "event {i} should be accepted"
            );
        }
    }

    // ── Spec scenario: Rate limit enforcement (spec line 131-133) ─────────────

    /// WHEN an agent emits 11 events within a 1-second sliding window (default
    /// limit: 10/s) THEN the 11th event MUST be rejected with
    /// AGENT_EVENT_RATE_EXCEEDED (spec line 133).
    #[test]
    fn eleventh_event_rejected() {
        let base = Instant::now();
        let mut limiter = AgentEventRateLimiter::new();

        // Send 10 events within 500ms — all should be accepted.
        for i in 0..10 {
            let t = ms_after(base, i * 50);
            assert!(
                limiter.check_and_record(t).is_ok(),
                "event {i} should be accepted"
            );
        }

        // 11th event within the same window — must be rejected.
        let t_11 = ms_after(base, 550);
        assert!(
            limiter.check_and_record(t_11).is_err(),
            "11th event must be rejected (AGENT_EVENT_RATE_EXCEEDED)"
        );
    }

    // ── Sliding window expiry ─────────────────────────────────────────────────

    /// After the window slides past old events, new events are accepted again.
    #[test]
    fn events_accepted_after_window_expires() {
        let base = Instant::now();
        let mut limiter = AgentEventRateLimiter::new();

        // Fill the window at t=0..500ms
        for i in 0..10 {
            let t = ms_after(base, i * 50);
            limiter.check_and_record(t).expect("should be accepted");
        }

        // Attempt at t=550ms — still within 1s of the first event (t=0), rejected.
        assert!(limiter.check_and_record(ms_after(base, 550)).is_err());

        // At t=1100ms — all events at t=0..500ms have expired (>1s ago).
        // New event should be accepted.
        assert!(
            limiter.check_and_record(ms_after(base, 1100)).is_ok(),
            "after window expiry, new events should be accepted"
        );
    }

    // ── Custom limit ──────────────────────────────────────────────────────────

    #[test]
    fn custom_limit_respected() {
        let base = Instant::now();
        let mut limiter = AgentEventRateLimiter::with_limit(3);

        // 3 accepted, 4th rejected.
        for i in 0..3 {
            assert!(limiter.check_and_record(ms_after(base, i * 50)).is_ok());
        }
        assert!(limiter.check_and_record(ms_after(base, 200)).is_err());
    }

    // ── Rate limit does not record rejected events ────────────────────────────

    /// Rejected events must NOT be recorded — subsequent events after expiry
    /// are unaffected by the rejected attempt.
    #[test]
    fn rejected_event_not_recorded() {
        let base = Instant::now();
        let mut limiter = AgentEventRateLimiter::with_limit(1);

        // First event accepted.
        limiter
            .check_and_record(base)
            .expect("first event accepted");
        // Second event in same window rejected.
        assert!(limiter.check_and_record(ms_after(base, 100)).is_err());

        // After window expiry, a fresh event is accepted.
        assert!(limiter.check_and_record(ms_after(base, 1100)).is_ok());
    }

    // ── current_count helper ─────────────────────────────────────────────────

    #[test]
    fn current_count_reflects_active_window() {
        let base = Instant::now();
        let mut limiter = AgentEventRateLimiter::new();

        limiter.check_and_record(base).unwrap();
        limiter.check_and_record(ms_after(base, 100)).unwrap();
        limiter.check_and_record(ms_after(base, 200)).unwrap();

        // At 300ms: all 3 events are within the 1-second window.
        assert_eq!(limiter.current_count(ms_after(base, 300)), 3);

        // At 999ms: all 3 events are still within the 1-second window.
        assert_eq!(limiter.current_count(ms_after(base, 999)), 3);

        // At 1001ms: the event at t=0 has expired (1001ms ≥ 1000ms).
        assert_eq!(limiter.current_count(ms_after(base, 1001)), 2);

        // At 1101ms: t=0 (1101ms) and t=100 (1001ms) have both expired.
        assert_eq!(limiter.current_count(ms_after(base, 1101)), 1);

        // At 1201ms: t=0, t=100, t=200 (1001ms ≥ 1000ms) have all expired; 0 remain.
        assert_eq!(limiter.current_count(ms_after(base, 1201)), 0);
    }
}
