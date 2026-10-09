//! Portal state, keyed by (agent id, portal id).
//!
//! [`PortalHub`] is the whole runtime model of `portal:<id>` surfaces: the
//! transcript, viewer replies, the input queue, liveness, and render pacing.
//! It is pure: every time-dependent call takes `now_us`, and nothing here
//! reads a clock, renders, or does I/O. The runtime's portal driver feeds it
//! MCP operations and composer submissions, sweeps it on wake deadlines, and
//! renders the portals [`PortalHub::take_due`] hands back.
//!
//! Identity is the authenticated agent id (PSK), so there are no owner
//! tokens: a second agent publishing to the same portal id gets its own
//! portal.

use std::collections::{BTreeMap, VecDeque};

/// A portal's identity: the owning agent and the id after `portal:`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortalKey {
    pub agent: String,
    pub id: String,
}

impl PortalKey {
    pub fn new(agent: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            id: id.into(),
        }
    }
}

/// Size, queue, and timing bounds. `Default` is the documented contract
/// (`docs/api.md`): degrade after 30 s without a publish, poll, or hold, and
/// reclaim 30 s after that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalLimits {
    /// Longest text one publish may carry.
    pub max_publish_bytes: usize,
    /// Transcript bytes kept; the oldest units are dropped past this.
    pub retained_bytes: usize,
    /// Newest transcript (and reply) bytes rendered.
    pub visible_bytes: usize,
    pub input_items: usize,
    pub input_item_bytes: usize,
    pub input_total_bytes: usize,
    pub input_ttl_us: u64,
    pub degrade_after_us: u64,
    pub reclaim_after_us: u64,
    /// Minimum spacing between two renders of one portal.
    pub render_interval_us: u64,
}

impl Default for PortalLimits {
    fn default() -> Self {
        Self {
            max_publish_bytes: 16 * 1024,
            retained_bytes: 256 * 1024,
            visible_bytes: 16 * 1024,
            input_items: 32,
            input_item_bytes: 4 * 1024,
            input_total_bytes: 32 * 1024,
            input_ttl_us: 10 * 60 * 1_000_000,
            degrade_after_us: 30 * 1_000_000,
            reclaim_after_us: 30 * 1_000_000,
            render_interval_us: 100_000,
        }
    }
}

/// Longest portal id, display name, or coalesce key.
const MAX_NAME_BYTES: usize = 128;

/// Why a portal call was refused. The MCP layer maps each to the shared
/// error set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortalError {
    /// The agent has no portal with this id (never attached, cleared, or
    /// reclaimed by the runtime).
    NotHeld,
    TooLarge {
        limit: usize,
    },
    InvalidArgument,
    QueueFull,
}

/// Lifecycle shown on the portal and returned by `hud_surfaces`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortalStatus {
    Attached,
    Active,
    Degraded,
    Detached,
}

impl PortalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Active => "active",
            Self::Degraded => "degraded",
            Self::Detached => "detached",
        }
    }

    /// Parse an agent-published `status` (the `hud_publish` schema enum).
    /// It only labels the portal; liveness is the runtime's.
    pub fn parse(s: &str) -> Result<Self, PortalError> {
        match s {
            "attached" => Ok(Self::Attached),
            "active" => Ok(Self::Active),
            "degraded" | "hud_unavailable" => Ok(Self::Degraded),
            "detached" => Ok(Self::Detached),
            _ => Err(PortalError::InvalidArgument),
        }
    }
}

/// One transcript unit (agent output) or viewer reply echo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    pub seq: u64,
    pub text: String,
    pub key: Option<String>,
    pub expects_reply: bool,
    pub at_us: u64,
}

/// One viewer reply waiting for the agent's `hud_input`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub id: String,
    pub text: String,
    pub at_us: u64,
    pub expires_us: u64,
    /// Returned by a poll at least once; stays queued until acked.
    pub delivered: bool,
}

#[derive(Clone, Debug)]
pub struct Portal {
    pub display_name: String,
    pub status: PortalStatus,
    pub transcript: VecDeque<Unit>,
    pub transcript_bytes: usize,
    pub replies: VecDeque<Unit>,
    pub inputs: VecDeque<Input>,
    /// Units appended since the last [`PortalHub::take_due`].
    pub unread: usize,
    pub last_seen_us: u64,
    /// `u64::MAX` holds until cleared.
    pub hold_until_us: Option<u64>,
    pub degraded_since_us: Option<u64>,
    /// Changed since the last render.
    pub dirty: bool,
    pub last_render_us: Option<u64>,
    /// Reclaimed by the runtime (viewer dismiss, idle reclaim): a marker so
    /// `hud_clear` stays an ok no-op and `hud_hold` is `NotHeld`. The next
    /// publish attaches a fresh portal.
    pub reclaimed_by_runtime: bool,
    next_seq: u64,
}

impl Portal {
    fn new(display_name: String, now_us: u64) -> Self {
        Self {
            display_name,
            status: PortalStatus::Attached,
            transcript: VecDeque::new(),
            transcript_bytes: 0,
            replies: VecDeque::new(),
            inputs: VecDeque::new(),
            unread: 0,
            last_seen_us: now_us,
            hold_until_us: None,
            degraded_since_us: None,
            dirty: true,
            last_render_us: None,
            reclaimed_by_runtime: false,
            next_seq: 1,
        }
    }

    fn tombstone() -> Self {
        let mut portal = Self::new(String::new(), 0);
        portal.status = PortalStatus::Detached;
        portal.reclaimed_by_runtime = true;
        portal.dirty = false;
        portal
    }

    fn live(&self) -> bool {
        !self.reclaimed_by_runtime
    }

    /// The newest transcript units that fit in `max_bytes` (at least one).
    pub fn visible_transcript(&self, max_bytes: usize) -> impl Iterator<Item = &Unit> {
        let start = newest_fit_start(&self.transcript, max_bytes);
        self.transcript.range(start..)
    }

    pub fn input_bytes(&self) -> usize {
        self.inputs.iter().map(|i| i.text.len()).sum()
    }

    /// An owner call (publish, poll, hold) proves the agent is live.
    fn touch(&mut self, now_us: u64) {
        self.last_seen_us = now_us;
        if self.degraded_since_us.take().is_some() {
            self.status = PortalStatus::Active;
            self.dirty = true;
        }
    }

    /// An upstream loss starts grace once, including portals not yet rendered.
    fn degrade(&mut self, now_us: u64) -> bool {
        if !self.live() || self.degraded_since_us.is_some() {
            return false;
        }
        self.degraded_since_us = Some(now_us);
        self.status = PortalStatus::Degraded;
        self.dirty = true;
        true
    }

    /// When an idle portal degrades; `None` while held until cleared.
    fn degrade_at(&self, limits: &PortalLimits) -> Option<u64> {
        match self.hold_until_us {
            Some(u64::MAX) => None,
            hold => Some(
                self.last_seen_us
                    .saturating_add(limits.degrade_after_us)
                    .max(hold.unwrap_or(0)),
            ),
        }
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }
}

fn newest_fit_start(units: &VecDeque<Unit>, max_bytes: usize) -> usize {
    let mut bytes = 0;
    let mut start = units.len();
    while start > 0 {
        let len = units[start - 1].text.len();
        if start < units.len() && bytes + len > max_bytes {
            break;
        }
        bytes += len;
        start -= 1;
    }
    start
}

/// The fields of one `hud_publish` to a portal.
#[derive(Clone, Debug, Default)]
pub struct Publish {
    pub display_name: Option<String>,
    pub text: Option<String>,
    /// Coalesce key: replaces the newest unit with the same key (D1).
    pub key: Option<String>,
    pub expects_reply: bool,
    pub status: Option<PortalStatus>,
}

/// One input item returned by [`PortalHub::poll_input`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolledInput {
    pub portal: String,
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputBatch {
    pub items: Vec<PolledInput>,
    /// Queued items that didn't fit `max`.
    pub remaining: usize,
}

/// Content-free summary for `hud_surfaces`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalSummary {
    pub id: String,
    pub status: PortalStatus,
    pub pending_input: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transition {
    Degraded(PortalKey),
    Reclaimed(PortalKey),
}

/// A portal ready to render, with the units appended since its last render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Due {
    pub key: PortalKey,
    pub unread: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeadlineKind {
    Render,
    Liveness,
    InputExpiry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline {
    pub at_us: u64,
    pub kind: DeadlineKind,
}

#[derive(Debug, Default)]
pub struct PortalHub {
    portals: BTreeMap<PortalKey, Portal>,
    next_input: u64,
    limits: PortalLimits,
}

impl PortalHub {
    pub fn new(limits: PortalLimits) -> Self {
        Self {
            portals: BTreeMap::new(),
            next_input: 0,
            limits,
        }
    }

    pub fn limits(&self) -> &PortalLimits {
        &self.limits
    }

    /// A live portal (not reclaimed).
    pub fn get(&self, key: &PortalKey) -> Option<&Portal> {
        self.portals.get(key).filter(|p| p.live())
    }

    fn live_mut(&mut self, key: &PortalKey) -> Result<&mut Portal, PortalError> {
        self.portals
            .get_mut(key)
            .filter(|p| p.live())
            .ok_or(PortalError::NotHeld)
    }

    /// Publish to a portal, attaching it first if the agent has none with
    /// this id. With neither text nor status it only attaches.
    pub fn publish(&mut self, key: &PortalKey, p: Publish, now_us: u64) -> Result<(), PortalError> {
        let name_ok = |s: &str| !s.is_empty() && s.len() <= MAX_NAME_BYTES;
        if !name_ok(&key.id)
            || !p.display_name.as_deref().is_none_or(name_ok)
            || !p.key.as_deref().is_none_or(name_ok)
        {
            return Err(PortalError::InvalidArgument);
        }
        let limit = self.limits.max_publish_bytes;
        if p.text.as_ref().is_some_and(|t| t.len() > limit) {
            return Err(PortalError::TooLarge { limit });
        }
        let retained = self.limits.retained_bytes;
        let portal = self
            .portals
            .entry(key.clone())
            .and_modify(|portal| {
                if !portal.live() {
                    *portal = Portal::new(key.id.clone(), now_us);
                }
            })
            .or_insert_with(|| Portal::new(key.id.clone(), now_us));
        portal.touch(now_us);
        portal.dirty = true;
        if let Some(name) = p.display_name {
            portal.display_name = name;
        }
        if let Some(text) = p.text {
            let replaced = p.key.as_ref().and_then(|k| {
                portal
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|u| u.key.as_ref() == Some(k))
            });
            if let Some(unit) = replaced {
                portal.transcript_bytes = portal.transcript_bytes - unit.text.len() + text.len();
                unit.text = text;
                unit.expects_reply = p.expects_reply;
                unit.at_us = now_us;
            } else {
                let seq = portal.next_seq();
                portal.transcript_bytes += text.len();
                portal.transcript.push_back(Unit {
                    seq,
                    text,
                    key: p.key,
                    expects_reply: p.expects_reply,
                    at_us: now_us,
                });
                portal.unread += 1;
            }
            while portal.transcript_bytes > retained && portal.transcript.len() > 1 {
                let dropped = portal.transcript.pop_front().expect("len > 1");
                portal.transcript_bytes -= dropped.text.len();
            }
            if portal.status == PortalStatus::Attached {
                portal.status = PortalStatus::Active;
            }
        }
        if let Some(status) = p.status {
            portal.status = status;
        }
        Ok(())
    }

    /// Keep a portal past the idle degrade for `ttl_ms` (0 = until cleared).
    pub fn hold(&mut self, key: &PortalKey, ttl_ms: u64, now_us: u64) -> Result<(), PortalError> {
        let portal = self.live_mut(key)?;
        portal.touch(now_us);
        portal.hold_until_us = Some(if ttl_ms == 0 {
            u64::MAX
        } else {
            now_us.saturating_add(ttl_ms.saturating_mul(1_000))
        });
        Ok(())
    }

    /// Detach a portal. Clearing one the runtime already reclaimed is a no-op
    /// success; one the agent never attached is `NotHeld`.
    pub fn clear(&mut self, key: &PortalKey) -> Result<(), PortalError> {
        self.portals
            .remove(key)
            .map(drop)
            .ok_or(PortalError::NotHeld)
    }

    /// Drop acked items, then return the agent's queued input across its
    /// portals, oldest first. Unacked items are returned again next time.
    /// Polling counts as liveness for every portal the agent holds.
    pub fn poll_input(
        &mut self,
        agent: &str,
        ack: &[String],
        max: Option<usize>,
        now_us: u64,
    ) -> InputBatch {
        let mut queued: Vec<(u64, &str, &mut Input)> = Vec::new();
        for (key, portal) in self.portals.iter_mut() {
            if key.agent != agent || !portal.live() {
                continue;
            }
            portal.touch(now_us);
            let before = portal.inputs.len();
            portal
                .inputs
                .retain(|i| !ack.contains(&i.id) && i.expires_us > now_us);
            if portal.inputs.len() != before {
                portal.dirty = true;
            }
            for input in portal.inputs.iter_mut() {
                if !input.delivered {
                    portal.dirty = true;
                }
                queued.push((input.at_us, key.id.as_str(), input));
            }
        }
        queued.sort_by_key(|(at, _, _)| *at);
        let take = max.unwrap_or(usize::MAX).min(queued.len());
        let remaining = queued.len() - take;
        let items = queued
            .into_iter()
            .take(take)
            .map(|(_, portal, input)| {
                input.delivered = true;
                PolledInput {
                    portal: portal.to_string(),
                    id: input.id.clone(),
                    text: input.text.clone(),
                }
            })
            .collect();
        InputBatch { items, remaining }
    }

    /// The agent's live portals.
    pub fn list(&self, agent: &str) -> Vec<PortalSummary> {
        self.portals
            .iter()
            .filter(|(key, p)| key.agent == agent && p.live())
            .map(|(key, p)| PortalSummary {
                id: key.id.clone(),
                status: p.status,
                pending_input: p.inputs.len(),
            })
            .collect()
    }

    /// Queue a viewer reply typed into the portal composer and echo it into
    /// the reply band. Returns the input id the agent will see.
    pub fn submit_reply(
        &mut self,
        key: &PortalKey,
        text: String,
        now_us: u64,
    ) -> Result<String, PortalError> {
        let limits = self.limits.clone();
        let portal = self.live_mut(key)?;
        if text.len() > limits.input_item_bytes {
            return Err(PortalError::TooLarge {
                limit: limits.input_item_bytes,
            });
        }
        if portal.inputs.len() >= limits.input_items
            || portal.input_bytes() + text.len() > limits.input_total_bytes
        {
            return Err(PortalError::QueueFull);
        }
        self.next_input += 1;
        let id = format!("i{}", self.next_input);
        let portal = self.live_mut(key)?;
        portal.inputs.push_back(Input {
            id: id.clone(),
            text: text.clone(),
            at_us: now_us,
            expires_us: now_us.saturating_add(limits.input_ttl_us),
            delivered: false,
        });
        let seq = portal.next_seq();
        portal.replies.push_back(Unit {
            seq,
            text,
            key: None,
            expects_reply: false,
            at_us: now_us,
        });
        let start = newest_fit_start(&portal.replies, limits.visible_bytes);
        portal.replies.drain(..start);
        portal.dirty = true;
        Ok(id)
    }

    /// Test support: queue input with a fixed id, without an echo.
    #[doc(hidden)]
    pub fn inject_input(
        &mut self,
        key: &PortalKey,
        id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<(), PortalError> {
        let portal = self.live_mut(key)?;
        portal.inputs.push_back(Input {
            id: id.into(),
            text: text.into(),
            at_us: 0,
            expires_us: u64::MAX,
            delivered: false,
        });
        portal.dirty = true;
        Ok(())
    }

    /// Degrade a portal now, regardless of hold (its upstream went away).
    pub fn degrade(&mut self, key: &PortalKey, now_us: u64) -> bool {
        match self.live_mut(key) {
            Ok(portal) => portal.degrade(now_us),
            Err(_) => false,
        }
    }

    /// Degrade all live portals when their shared upstream goes away.
    /// Returns only changed identities; holds cannot keep a disconnected stream live.
    pub fn degrade_all(&mut self, now_us: u64) -> Vec<PortalKey> {
        let mut changed = Vec::new();
        for (key, portal) in &mut self.portals {
            if portal.degrade(now_us) {
                changed.push(key.clone());
            }
        }
        changed
    }

    /// The runtime took the portal away (viewer dismiss, lost surface).
    pub fn reclaim(&mut self, key: &PortalKey) {
        if let Some(portal) = self.portals.get_mut(key) {
            *portal = Portal::tombstone();
        }
    }

    /// Expire input, degrade idle portals, and reclaim long-degraded ones.
    pub fn sweep(&mut self, now_us: u64) -> Vec<Transition> {
        let limits = &self.limits;
        let mut transitions = Vec::new();
        for (key, portal) in self.portals.iter_mut().filter(|(_, p)| p.live()) {
            let before = portal.inputs.len();
            portal.inputs.retain(|i| i.expires_us > now_us);
            if portal.inputs.len() != before {
                portal.dirty = true;
            }
            match portal.degraded_since_us {
                None if portal.degrade_at(limits).is_some_and(|at| now_us >= at) => {
                    portal.degraded_since_us = Some(now_us);
                    portal.status = PortalStatus::Degraded;
                    portal.dirty = true;
                    transitions.push(Transition::Degraded(key.clone()));
                }
                Some(since) if now_us >= since.saturating_add(limits.reclaim_after_us) => {
                    *portal = Portal::tombstone();
                    transitions.push(Transition::Reclaimed(key.clone()));
                }
                _ => {}
            }
        }
        transitions
    }

    /// Portals with changes whose render interval has passed. Clears their
    /// dirty flag and unread count.
    pub fn take_due(&mut self, now_us: u64) -> Vec<Due> {
        let interval = self.limits.render_interval_us;
        self.portals
            .iter_mut()
            .filter(|(_, p)| {
                p.live()
                    && p.dirty
                    && p.last_render_us
                        .is_none_or(|t| now_us >= t.saturating_add(interval))
            })
            .map(|(key, portal)| {
                portal.dirty = false;
                portal.last_render_us = Some(now_us);
                Due {
                    key: key.clone(),
                    unread: std::mem::take(&mut portal.unread),
                }
            })
            .collect()
    }

    /// The earliest instant the hub needs `take_due` or `sweep` again.
    pub fn next_deadline(&self) -> Option<Deadline> {
        let interval = self.limits.render_interval_us;
        self.portals
            .values()
            .filter(|p| p.live())
            .flat_map(|p| {
                let render = p.dirty.then(|| Deadline {
                    at_us: p.last_render_us.map_or(0, |t| t.saturating_add(interval)),
                    kind: DeadlineKind::Render,
                });
                let liveness = match p.degraded_since_us {
                    Some(since) => Some(since.saturating_add(self.limits.reclaim_after_us)),
                    None => p.degrade_at(&self.limits),
                }
                .map(|at_us| Deadline {
                    at_us,
                    kind: DeadlineKind::Liveness,
                });
                let expiry = p
                    .inputs
                    .iter()
                    .map(|i| i.expires_us)
                    .min()
                    .map(|at_us| Deadline {
                        at_us,
                        kind: DeadlineKind::InputExpiry,
                    });
                [render, liveness, expiry]
            })
            .flatten()
            .min_by_key(|d| (d.at_us, d.kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000;

    fn key(agent: &str, id: &str) -> PortalKey {
        PortalKey::new(agent, id)
    }

    fn text(t: &str) -> Publish {
        Publish {
            text: Some(t.to_string()),
            ..Publish::default()
        }
    }

    fn texts(hub: &PortalHub, k: &PortalKey) -> Vec<String> {
        hub.get(k)
            .unwrap()
            .transcript
            .iter()
            .map(|u| u.text.clone())
            .collect()
    }

    #[test]
    fn publish_attaches_implicitly_and_lists_for_owner_only() {
        let mut hub = PortalHub::default();
        let a = key("alice", "main");
        hub.publish(&a, Publish::default(), 1).unwrap();
        assert_eq!(hub.get(&a).unwrap().status, PortalStatus::Attached);
        assert!(hub.get(&a).unwrap().transcript.is_empty());
        hub.publish(&a, text("hi"), 2).unwrap();
        assert_eq!(hub.get(&a).unwrap().status, PortalStatus::Active);
        assert_eq!(
            hub.list("alice"),
            [PortalSummary {
                id: "main".into(),
                status: PortalStatus::Active,
                pending_input: 0,
            }]
        );
        assert!(hub.list("bob").is_empty());
        assert_eq!(
            hub.publish(&a, text(&"x".repeat(16 * 1024 + 1)), 3),
            Err(PortalError::TooLarge { limit: 16 * 1024 })
        );
        // Every `status` in the hud_publish schema enum labels a live portal.
        for status in [
            "attached",
            "active",
            "degraded",
            "hud_unavailable",
            "detached",
        ] {
            let status = Some(PortalStatus::parse(status).unwrap());
            hub.publish(
                &a,
                Publish {
                    status,
                    ..Publish::default()
                },
                4,
            )
            .unwrap();
            assert_eq!(hub.list("alice").len(), 1);
        }
        assert_eq!(
            PortalStatus::parse("busy"),
            Err(PortalError::InvalidArgument)
        );
    }

    #[test]
    fn same_portal_id_different_agents_are_distinct() {
        let mut hub = PortalHub::default();
        let (a, b) = (key("alice", "main"), key("bob", "main"));
        hub.publish(&a, text("from alice"), 1).unwrap();
        hub.publish(&b, text("from bob"), 1).unwrap();
        hub.inject_input(&a, "i-a", "for alice").unwrap();
        assert_eq!(texts(&hub, &a), ["from alice"]);
        assert_eq!(texts(&hub, &b), ["from bob"]);
        assert!(hub.poll_input("bob", &[], None, 2).items.is_empty());
        hub.clear(&b).unwrap();
        assert_eq!(texts(&hub, &a), ["from alice"], "bob's clear is his own");
        assert_eq!(hub.poll_input("alice", &[], None, 2).items.len(), 1);
    }

    #[test]
    fn key_replaces_newest_unit_with_same_key() {
        let mut hub = PortalHub::default();
        let k = key("a", "p");
        let keyed = |t: &str| Publish {
            key: Some("progress".into()),
            ..text(t)
        };
        hub.publish(&k, keyed("10%"), 1).unwrap();
        hub.publish(&k, text("log line"), 2).unwrap();
        hub.publish(&k, keyed("50%"), 3).unwrap();
        assert_eq!(texts(&hub, &k), ["50%", "log line"], "replaced in place");
        let p = hub.get(&k).unwrap();
        assert_eq!(p.transcript_bytes, "50%".len() + "log line".len());
        assert_eq!(p.transcript[0].at_us, 3);
        assert_eq!(hub.take_due(3)[0].unread, 2, "a replacement is not new");
    }

    #[test]
    fn transcript_retention_trims_head_past_256k_keeps_16k_visible() {
        let mut hub = PortalHub::default();
        let k = key("a", "p");
        let chunk = "x".repeat(10 * 1024);
        for i in 0..30u64 {
            hub.publish(&k, text(&format!("{i:02}{chunk}")), i).unwrap();
        }
        let p = hub.get(&k).unwrap();
        assert!(p.transcript_bytes <= 256 * 1024);
        assert_eq!(
            p.transcript_bytes,
            p.transcript.iter().map(|u| u.text.len()).sum::<usize>()
        );
        assert_eq!(p.transcript.len(), 25, "the 5 oldest units were dropped");
        assert!(p.transcript[0].text.starts_with("05"));
        let visible: Vec<_> = p.visible_transcript(16 * 1024).collect();
        assert_eq!(visible.len(), 1, "only the newest 10 KiB unit fits 16 KiB");
        assert!(visible[0].text.starts_with("29"));
    }

    #[test]
    fn input_redelivered_until_acked() {
        let mut hub = PortalHub::default();
        let k = key("a", "p");
        hub.publish(&k, text("ask"), 0).unwrap();
        let id = hub.submit_reply(&k, "yes".into(), 1).unwrap();
        hub.submit_reply(&k, "and more".into(), 2).unwrap();
        let first = hub.poll_input("a", &[], Some(1), 3);
        assert_eq!(first.items[0].id, id);
        assert_eq!(first.items[0].portal, "p");
        assert_eq!(first.remaining, 1);
        let again = hub.poll_input("a", &[], None, 4);
        assert_eq!(again.items.len(), 2, "unacked input comes back");
        assert_eq!(again.items[0].id, id, "oldest first");
        let after = hub.poll_input("a", std::slice::from_ref(&id), None, 5);
        assert_eq!(after.items.len(), 1);
        assert_eq!(after.items[0].text, "and more");
        assert_eq!(hub.get(&k).unwrap().replies.len(), 2, "replies are echoed");
    }

    #[test]
    fn input_caps_and_ttl_expiry() {
        let mut hub = PortalHub::default();
        let k = key("a", "p");
        hub.publish(&k, text("ask"), 0).unwrap();
        assert_eq!(
            hub.submit_reply(&k, "x".repeat(4 * 1024 + 1), 0),
            Err(PortalError::TooLarge { limit: 4 * 1024 })
        );
        for _ in 0..32 {
            hub.submit_reply(&k, "y".into(), 0).unwrap();
        }
        assert_eq!(
            hub.submit_reply(&k, "y".into(), 0),
            Err(PortalError::QueueFull)
        );
        hub.poll_input("a", &[], None, 0);
        let ids: Vec<String> = hub
            .get(&k)
            .unwrap()
            .inputs
            .iter()
            .map(|i| i.id.clone())
            .collect();
        hub.poll_input("a", &ids[..25], None, 0);
        for _ in 0..7 {
            hub.submit_reply(&k, "z".repeat(4 * 1024), 0).unwrap();
        }
        assert_eq!(
            hub.submit_reply(&k, "z".repeat(4 * 1024), 0),
            Err(PortalError::QueueFull),
            "32 KiB total"
        );
        assert_eq!(
            hub.submit_reply(&key("a", "never"), "q".into(), 0),
            Err(PortalError::NotHeld)
        );

        let mut hub = PortalHub::default();
        hub.publish(&k, Publish::default(), 0).unwrap();
        hub.hold(&k, 0, 0).unwrap();
        hub.submit_reply(&k, "late".into(), 1).unwrap();
        let ttl = 10 * 60 * S;
        assert_eq!(
            hub.next_deadline(),
            Some(Deadline {
                at_us: 0,
                kind: DeadlineKind::Render
            })
        );
        hub.take_due(1);
        assert_eq!(
            hub.next_deadline(),
            Some(Deadline {
                at_us: 1 + ttl,
                kind: DeadlineKind::InputExpiry
            })
        );
        hub.sweep(ttl);
        assert_eq!(hub.get(&k).unwrap().inputs.len(), 1);
        hub.sweep(ttl + 1);
        assert!(hub.get(&k).unwrap().inputs.is_empty(), "expired at its TTL");
        assert!(
            hub.get(&k).unwrap().dirty,
            "expiry repaints the pending count"
        );
    }

    #[test]
    fn idle_portal_degrades_at_30s_and_reclaims_30s_later() {
        let mut hub = PortalHub::default();
        let k = key("a", "p");
        hub.publish(&k, text("t"), S).unwrap();
        assert_eq!(hub.sweep(31 * S - 1), []);
        assert_eq!(
            hub.next_deadline().map(|d| (d.at_us, d.kind)),
            Some((0, DeadlineKind::Render))
        );
        hub.take_due(S);
        assert_eq!(
            hub.next_deadline(),
            Some(Deadline {
                at_us: 31 * S,
                kind: DeadlineKind::Liveness
            })
        );
        assert_eq!(hub.sweep(31 * S), [Transition::Degraded(k.clone())]);
        assert_eq!(hub.get(&k).unwrap().status, PortalStatus::Degraded);
        assert_eq!(hub.list("a")[0].status, PortalStatus::Degraded);
        assert_eq!(hub.sweep(61 * S - 1), []);
        assert_eq!(hub.sweep(61 * S), [Transition::Reclaimed(k.clone())]);
        assert!(hub.get(&k).is_none());
        assert!(hub.list("a").is_empty());
        assert_eq!(hub.hold(&k, 0, 62 * S), Err(PortalError::NotHeld));
        assert_eq!(
            hub.clear(&k),
            Ok(()),
            "clearing a reclaimed portal is a no-op"
        );
    }

    #[test]
    fn poll_or_hold_refreshes_liveness() {
        let mut hub = PortalHub::default();
        let k = key("a", "p");
        hub.publish(&k, text("t"), 0).unwrap();
        hub.poll_input("a", &[], None, 20 * S);
        assert_eq!(
            hub.sweep(49 * S),
            [],
            "the poll at 20 s moved the degrade to 50 s"
        );
        assert_eq!(hub.sweep(50 * S), [Transition::Degraded(k.clone())]);
        hub.hold(&k, 1, 55 * S).unwrap();
        let p = hub.get(&k).unwrap();
        assert_eq!(
            p.degraded_since_us, None,
            "an owner call recovers a degraded portal"
        );
        assert_eq!(p.status, PortalStatus::Active);
        assert_eq!(hub.sweep(84 * S), []);
        assert_eq!(hub.sweep(85 * S), [Transition::Degraded(k)]);
    }

    #[test]
    fn hold_ttl_outlives_reap_window_then_lapses() {
        let mut hub = PortalHub::default();
        let k = key("a", "p");
        hub.publish(&k, text("kept"), 0).unwrap();
        hub.take_due(0);
        hub.hold(&k, 300_000, 0).unwrap();
        assert_eq!(
            hub.next_deadline(),
            Some(Deadline {
                at_us: 300 * S,
                kind: DeadlineKind::Liveness
            })
        );
        assert_eq!(
            hub.sweep(120 * S),
            [],
            "past the 60 s idle reap, inside the hold"
        );
        assert_eq!(texts(&hub, &k), ["kept"]);
        assert_eq!(hub.sweep(300 * S), [Transition::Degraded(k.clone())]);
        assert_eq!(hub.sweep(330 * S), [Transition::Reclaimed(k)]);
    }

    #[test]
    fn hold_zero_never_degrades() {
        let mut hub = PortalHub::default();
        let k = key("a", "p");
        hub.publish(&k, text("t"), 0).unwrap();
        hub.take_due(0);
        hub.hold(&k, 0, 0).unwrap();
        assert_eq!(hub.next_deadline(), None);
        assert_eq!(hub.sweep(u64::MAX - 1), []);
        assert_eq!(hub.hold(&key("b", "p"), 0, 0), Err(PortalError::NotHeld));
    }

    #[test]
    fn clear_frees_id_for_fresh_attach() {
        let mut hub = PortalHub::default();
        let k = key("a", "p");
        hub.publish(&k, text("old"), 0).unwrap();
        hub.submit_reply(&k, "r".into(), 0).unwrap();
        hub.clear(&k).unwrap();
        assert_eq!(hub.clear(&k), Err(PortalError::NotHeld));
        hub.publish(&k, text("new"), 1).unwrap();
        assert_eq!(texts(&hub, &k), ["new"]);
        assert!(hub.get(&k).unwrap().inputs.is_empty());

        hub.reclaim(&k);
        hub.publish(&k, text("after reclaim"), 2).unwrap();
        assert_eq!(texts(&hub, &k), ["after reclaim"]);
    }

    #[test]
    fn take_due_paces_renders_at_the_render_interval() {
        let mut hub = PortalHub::default();
        let k = key("a", "p");
        hub.publish(&k, text("1"), 0).unwrap();
        assert_eq!(
            hub.take_due(0),
            [Due {
                key: k.clone(),
                unread: 1
            }]
        );
        hub.publish(&k, text("2"), 10).unwrap();
        hub.publish(&k, text("3"), 20).unwrap();
        assert_eq!(hub.take_due(99_999), [], "inside the 100 ms interval");
        assert_eq!(
            hub.next_deadline(),
            Some(Deadline {
                at_us: 100_000,
                kind: DeadlineKind::Render
            })
        );
        assert_eq!(hub.take_due(100_000), [Due { key: k, unread: 2 }]);
        assert_eq!(hub.take_due(300_000), [], "nothing changed");
    }
}
