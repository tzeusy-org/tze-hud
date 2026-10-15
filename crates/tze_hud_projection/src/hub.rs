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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// Projection lifecycle visible through bounded status summaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionLifecycleState {
    Attached,
    Active,
    Degraded,
    HudUnavailable,
    Detached,
    CleanupPending,
    Expired,
}

/// Output kind for published transcript units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    #[default]
    Assistant,
    /// Text submitted by the on-screen viewer via the HUD composer. Echoed into
    /// the reply history by `PortalHub::submit_reply` so the conversation is not
    /// structurally one-sided on the portal surface.
    Viewer,
}

/// Daemon-owned input delivery state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDeliveryState {
    Delivered,
}

/// Expanded or collapsed projected-portal presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectedPortalPresentation {
    Expanded,
    Collapsed,
}

/// Only the local composer's presentation status crosses the render boundary.
/// Draft text, selection and caret remain in ComposerDraftManager.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ComposerStatus {
    #[default]
    Ready,
    Composing,
    AtCapacity,
}

/// Local-first HUD composer feedback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortalInputFeedbackState {
    Accepted,
    Rejected,
}

/// Local composer result; the driver uses the Hub's canonical refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalInputFeedback {
    pub projection_id: String,
    pub input_id: String,
    pub feedback_state: PortalInputFeedbackState,
    pub error_code: Option<PortalError>,
    pub pending_input_count: usize,
    pub pending_input_bytes: usize,
    pub status_summary: String,
}

/// One rendered turn, carrying the Hub's runtime-assigned arrival time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptUnit {
    pub sequence: u64,
    pub output_text: String,
    pub output_kind: OutputKind,
    pub expects_reply: bool,
    pub appended_at_wall_us: u64,
}

/// The renderer's view of one Hub portal. It carries presentation data only;
/// publishing, ownership, input delivery and cadence remain in PortalHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalView {
    pub portal_id: String,
    pub presentation: ProjectedPortalPresentation,
    pub connection_degraded: bool,
    pub has_ever_connected: bool,
    pub interaction_enabled: bool,
    pub composer: ComposerStatus,
    pub display_name: Option<String>,
    pub lifecycle_state: Option<ProjectionLifecycleState>,
    pub status_text: Option<String>,
    pub visible_transcript: Vec<TranscriptUnit>,
    pub input_history: Vec<TranscriptUnit>,
    pub unread_output_count: Option<usize>,
    pub visible_unread_output_count: Option<usize>,
    pub pending_input_count: Option<usize>,
    pub latest_viewer_delivery_state: Option<InputDeliveryState>,
    pub last_input_feedback: Option<PortalInputFeedback>,
}

// ─── Portal geometry update types (hud-5jbra.9) ──────────────────────────────

/// Latest runtime resize in integer display pixels. Geometry is local to the
/// driver; it is not part of the renderer view or an agent payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterPortalRect {
    /// Left edge of the portal in display pixels (rounded from f32).
    pub x_px: i32,
    /// Top edge of the portal in display pixels (rounded from f32).
    pub y_px: i32,
    /// Width of the portal in display pixels (rounded from f32, ≥ 0).
    pub width_px: i32,
    /// Height of the portal in display pixels (rounded from f32, ≥ 0).
    pub height_px: i32,
}

impl AdapterPortalRect {
    /// Construct from float display-space coordinates.
    ///
    /// Width and height are clamped to 0 before rounding to avoid negative
    /// values from rounding artefacts at the minimum-clamped boundary.
    pub fn from_f32(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x_px: x.round() as i32,
            y_px: y.round() as i32,
            width_px: width.max(0.0).round() as i32,
            height_px: height.max(0.0).round() as i32,
        }
    }
}

/// Coalescible adapter-facing geometry snapshot (§6b.4).
///
/// Delivered to the owning adapter when portal geometry changes due to a
/// pointer gesture or hotkey resize. The adapter MUST NOT apply `publish_geometry`
/// while `gesture_active == true` — gesture snapshots are authoritative.
///
/// Message class: **state-stream**. Older snapshots for the same portal MUST be
/// discarded when a newer one arrives within the same adapter delivery window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterGeometrySnapshot {
    /// Final clamped portal bounds at this gesture step.
    pub rect: AdapterPortalRect,
    /// True while a pointer gesture is active (gesture is authoritative).
    /// False on hotkey resize or gesture end (adapter may resume publishing).
    pub gesture_active: bool,
    /// Monotonic sequence counter — allows the adapter to detect skipped
    /// snapshots when the transport does not deliver every event.
    pub sequence: u64,
}

/// Batched adapter geometry notification for state-stream coalescing (§6b.4).
///
/// Replaces the `latest` field on each new snapshot (latest-wins). The adapter
/// always reads `latest` if present to update portal geometry.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AdapterGeometryBatch {
    /// Latest geometry snapshot (coalescible — newer replaces older).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<AdapterGeometrySnapshot>,
}

impl AdapterGeometryBatch {
    /// Coalesce a geometry snapshot (latest-wins by sequence number).
    pub fn coalesce(&mut self, snapshot: AdapterGeometrySnapshot) {
        match &self.latest {
            Some(existing) if snapshot.sequence > existing.sequence => {
                self.latest = Some(snapshot);
            }
            None => {
                self.latest = Some(snapshot);
            }
            _ => {}
        }
    }

    /// True if there is a snapshot to deliver.
    pub fn is_empty(&self) -> bool {
        self.latest.is_none()
    }
}

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
            "active" => Ok(Self::Active),
            "degraded" => Ok(Self::Degraded),
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
    incarnation: u64,
    pub display_name: String,
    pub status: PortalStatus,
    pub transcript: VecDeque<Unit>,
    pub transcript_bytes: usize,
    pub replies: VecDeque<Unit>,
    pub inputs: VecDeque<Input>,
    /// Units appended since the last [`PortalHub::take_due`].
    pub unread: usize,
    /// Latest nonempty rendered batch's badge, retained across local repaints.
    pub rendered_unread: usize,
    /// One-shot runtime-requested repaint when a rendered activity cue expires.
    pub activity_cue_due_us: Option<u64>,
    pub last_seen_us: u64,
    /// `u64::MAX` holds until cleared.
    pub hold_until_us: Option<u64>,
    pub degraded_since_us: Option<u64>,
    /// Changed since the last render.
    pub dirty: bool,
    pub last_render_us: Option<u64>,
    /// Accepted reply or truthful rejection feedback bypasses pacing once.
    local_feedback_pending: bool,
    /// Reclaimed by the runtime (viewer dismiss, idle reclaim): a marker so
    /// `hud_clear` stays an ok no-op and `hud_hold` is `NotHeld`. The next
    /// publish attaches a fresh portal.
    pub reclaimed_by_runtime: bool,
    next_seq: u64,
}

impl Portal {
    fn new(display_name: String, now_us: u64) -> Self {
        Self {
            incarnation: 0,
            display_name,
            status: PortalStatus::Attached,
            transcript: VecDeque::new(),
            transcript_bytes: 0,
            replies: VecDeque::new(),
            inputs: VecDeque::new(),
            unread: 0,
            rendered_unread: 0,
            activity_cue_due_us: None,
            last_seen_us: now_us,
            hold_until_us: None,
            degraded_since_us: None,
            dirty: true,
            last_render_us: None,
            local_feedback_pending: false,
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

/// Facts from the completed publish, without transcript or reply content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishOutcome {
    pub reattached: bool,
    pub pending_input: usize,
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
    ActivityCue,
    Liveness,
    InputExpiry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline {
    pub at_us: u64,
    pub kind: DeadlineKind,
}

#[derive(Debug)]
struct ActiveInputPoll {
    expires_us: u64,
    active: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
pub struct PortalHub {
    portals: BTreeMap<PortalKey, Portal>,
    next_incarnation: u64,
    next_input: u64,
    // Finite request ownership is independent of whether a portal exists yet.
    active_input_polls: BTreeMap<String, BTreeMap<u64, ActiveInputPoll>>,
    next_input_poll: u64,
    limits: PortalLimits,
}

fn hold_deadline(ttl_ms: u64, now_us: u64) -> u64 {
    if ttl_ms == 0 {
        u64::MAX
    } else {
        now_us.saturating_add(ttl_ms.saturating_mul(1_000))
    }
}

impl PortalHub {
    pub fn new(limits: PortalLimits) -> Self {
        Self {
            portals: BTreeMap::new(),
            next_incarnation: 0,
            next_input: 0,
            active_input_polls: BTreeMap::new(),
            next_input_poll: 0,
            limits,
        }
    }

    pub fn limits(&self) -> &PortalLimits {
        &self.limits
    }

    /// A finite active poll defers only its agent's idle degradation. It does
    /// not renew last_seen, extend input expiry or override an upstream loss.
    pub fn begin_input_poll(
        &mut self,
        agent: &str,
        remaining_us: u64,
        now_us: u64,
        active: Arc<AtomicBool>,
    ) -> Result<Option<u64>, PortalError> {
        let expires_us = now_us.saturating_add(remaining_us);
        if remaining_us == 0 || expires_us <= now_us {
            return Ok(None);
        }
        self.next_input_poll = self
            .next_input_poll
            .checked_add(1)
            .ok_or(PortalError::QueueFull)?;
        let request = self.next_input_poll;
        self.active_input_polls
            .entry(agent.to_string())
            .or_default()
            .insert(request, ActiveInputPoll { expires_us, active });
        Ok(Some(request))
    }

    /// Release just this request. Cancellation is not an agent heartbeat.
    pub fn end_input_poll(&mut self, agent: &str, request: u64) {
        if let Some(polls) = self.active_input_polls.get_mut(agent) {
            polls.remove(&request);
            if polls.is_empty() {
                self.active_input_polls.remove(agent);
            }
        }
    }

    /// A live portal (not reclaimed).
    pub fn get(&self, key: &PortalKey) -> Option<&Portal> {
        self.portals.get(key).filter(|p| p.live())
    }

    /// Identifies one attachment across clear/re-publish, but not a resume.
    pub fn incarnation(&self, key: &PortalKey) -> Option<u64> {
        self.get(key).map(|p| p.incarnation)
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
        self.publish_with_hold(key, p, None, now_us).map(|_| ())
    }

    /// Publish and optionally hold in one operation. A runtime-reclaimed id
    /// attaches fresh; an explicitly cleared id is a first attachment again.
    pub fn publish_with_hold(
        &mut self,
        key: &PortalKey,
        p: Publish,
        ttl_ms: Option<u64>,
        now_us: u64,
    ) -> Result<PublishOutcome, PortalError> {
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
        let reattached = self
            .portals
            .get(key)
            .is_some_and(|p| p.reclaimed_by_runtime);
        if self.get(key).is_none() {
            self.next_incarnation = self
                .next_incarnation
                .checked_add(1)
                .ok_or(PortalError::InvalidArgument)?;
            let mut portal = Portal::new(key.id.clone(), now_us);
            portal.incarnation = self.next_incarnation;
            self.portals.insert(key.clone(), portal);
        }
        let portal = self.portals.get_mut(key).expect("attached above");
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
        if let Some(ttl_ms) = ttl_ms {
            portal.hold_until_us = Some(hold_deadline(ttl_ms, now_us));
        }
        Ok(PublishOutcome {
            reattached,
            pending_input: portal.inputs.len(),
        })
    }

    /// Keep a portal past the idle degrade for `ttl_ms` (0 = until cleared).
    pub fn hold(&mut self, key: &PortalKey, ttl_ms: u64, now_us: u64) -> Result<(), PortalError> {
        let portal = self.live_mut(key)?;
        portal.touch(now_us);
        portal.hold_until_us = Some(hold_deadline(ttl_ms, now_us));
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
        self.poll_input_with_changes(agent, ack, max, now_us).0
    }

    /// The ordinary input batch plus whether this pass changed rendered state.
    /// An existing dirty portal, or another agent's portal, is not a change.
    pub fn poll_input_with_changes(
        &mut self,
        agent: &str,
        ack: &[String],
        max: Option<usize>,
        now_us: u64,
    ) -> (InputBatch, bool) {
        let mut changed = false;
        let mut queued: Vec<(u64, &str, &mut Input)> = Vec::new();
        for (key, portal) in self.portals.iter_mut() {
            if key.agent != agent || !portal.live() {
                continue;
            }
            changed |= portal.degraded_since_us.is_some();
            let was_dirty = portal.dirty;
            portal.touch(now_us);
            let before = portal.inputs.len();
            portal
                .inputs
                .retain(|i| !ack.contains(&i.id) && i.expires_us > now_us);
            if portal.inputs.len() != before {
                portal.dirty = true;
                changed = true;
            }
            for input in portal.inputs.iter_mut() {
                if !input.delivered {
                    portal.dirty = true;
                }
                queued.push((input.at_us, key.id.as_str(), input));
            }
            changed |= !was_dirty && portal.dirty;
        }
        queued.sort_by_key(|(at, _, _)| *at);
        let take = max.unwrap_or(usize::MAX).min(queued.len());
        let remaining = queued.len() - take;
        let items = queued
            .into_iter()
            .take(take)
            .map(|(_, portal, input)| {
                changed |= !input.delivered;
                input.delivered = true;
                PolledInput {
                    portal: portal.to_string(),
                    id: input.id.clone(),
                    text: input.text.clone(),
                }
            })
            .collect();
        (InputBatch { items, remaining }, changed)
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
        portal.local_feedback_pending = true;
        portal.dirty = true;
        Ok(id)
    }

    /// Paint local rejection feedback only for the attachment that supplied it.
    /// This adds no accepted input, transcript unit or agent notification.
    pub fn mark_local_feedback(&mut self, key: &PortalKey, incarnation: u64) -> bool {
        let Ok(portal) = self.live_mut(key) else {
            return false;
        };
        if portal.incarnation != incarnation {
            return false;
        }
        portal.local_feedback_pending = true;
        portal.dirty = true;
        true
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

    /// The owner cleared the agent: reclaim every live portal it holds, as a
    /// viewer dismiss would. Returns how many.
    pub fn reclaim_agent(&mut self, agent: &str) -> usize {
        let mut reclaimed = 0;
        for (key, portal) in &mut self.portals {
            if key.agent == agent && portal.live() {
                *portal = Portal::tombstone();
                reclaimed += 1;
            }
        }
        reclaimed
    }

    /// Live portals per agent.
    pub fn live_counts(&self) -> std::collections::BTreeMap<String, usize> {
        let mut counts = std::collections::BTreeMap::new();
        for key in self
            .portals
            .iter()
            .filter(|(_, p)| p.live())
            .map(|(k, _)| k)
        {
            *counts.entry(key.agent.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// Expire input, degrade idle portals, and reclaim long-degraded ones.
    pub fn sweep(&mut self, now_us: u64) -> Vec<Transition> {
        self.active_input_polls.retain(|_, polls| {
            polls.retain(|_, poll| poll.active.load(Ordering::Acquire) && poll.expires_us > now_us);
            !polls.is_empty()
        });
        let limits = &self.limits;
        let mut transitions = Vec::new();
        for (key, portal) in self.portals.iter_mut().filter(|(_, p)| p.live()) {
            let before = portal.inputs.len();
            portal.inputs.retain(|i| i.expires_us > now_us);
            if portal.inputs.len() != before {
                portal.dirty = true;
            }
            match portal.degraded_since_us {
                None if !self
                    .active_input_polls
                    .get(&key.agent)
                    .is_some_and(|polls| {
                        polls.values().any(|poll| {
                            poll.active.load(Ordering::Acquire) && poll.expires_us > now_us
                        })
                    })
                    && portal.degrade_at(limits).is_some_and(|at| now_us >= at) =>
                {
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

    /// Portals with changes whose render interval has passed, or an accepted
    /// local reply to show immediately. Clears dirty, local urgency and unread.
    pub fn take_due(&mut self, now_us: u64) -> Vec<Due> {
        let interval = self.limits.render_interval_us;
        self.portals
            .iter_mut()
            .filter(|(_, p)| {
                p.live()
                    && ((p.dirty
                        && (p.local_feedback_pending
                            || p.last_render_us
                                .is_none_or(|t| now_us >= t.saturating_add(interval))))
                        || p.activity_cue_due_us.is_some_and(|due| now_us >= due))
            })
            .map(|(key, portal)| {
                portal.activity_cue_due_us = None;
                if portal.unread > 0 {
                    portal.rendered_unread = portal.unread;
                }
                portal.dirty = false;
                portal.local_feedback_pending = false;
                portal.last_render_us = Some(now_us);
                Due {
                    key: key.clone(),
                    unread: std::mem::take(&mut portal.unread),
                }
            })
            .collect()
    }

    /// Schedule the single repaint needed to clear a cue that was actually drawn.
    pub fn set_activity_cue_deadline(
        &mut self,
        key: &PortalKey,
        incarnation: u64,
        due_us: Option<u64>,
    ) {
        if let Some(portal) = self
            .portals
            .get_mut(key)
            .filter(|p| p.incarnation == incarnation)
        {
            portal.activity_cue_due_us = due_us;
        }
    }

    /// The earliest instant the hub needs `take_due` or `sweep` again.
    pub fn next_deadline(&self) -> Option<Deadline> {
        let interval = self.limits.render_interval_us;
        self.portals
            .iter()
            .filter(|(_, p)| p.live())
            .flat_map(|(key, p)| {
                let render = p.dirty.then(|| Deadline {
                    at_us: if p.local_feedback_pending {
                        0
                    } else {
                        p.last_render_us.map_or(0, |t| t.saturating_add(interval))
                    },
                    kind: DeadlineKind::Render,
                });
                let liveness = match p.degraded_since_us {
                    Some(since) => Some(since.saturating_add(self.limits.reclaim_after_us)),
                    None => p.degrade_at(&self.limits).map(|at| {
                        self.active_input_polls
                            .get(&key.agent)
                            .and_then(|polls| {
                                polls
                                    .values()
                                    .filter(|poll| poll.active.load(Ordering::Acquire))
                                    .map(|poll| poll.expires_us)
                                    .max()
                            })
                            .map_or(at, |expires_us| at.max(expires_us))
                    }),
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
                let activity = p.activity_cue_due_us.map(|at_us| Deadline {
                    at_us,
                    kind: DeadlineKind::ActivityCue,
                });
                [render, activity, liveness, expiry]
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
        for status in ["active", "degraded"] {
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
        for status in ["attached", "detached", "hud_unavailable", "busy"] {
            assert_eq!(
                PortalStatus::parse(status),
                Err(PortalError::InvalidArgument)
            );
        }
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
        let first = hub.get(&k).unwrap().transcript[0].seq;
        hub.publish(&k, keyed("100%"), 4).unwrap();
        let p = hub.get(&k).unwrap();
        assert_eq!(
            p.transcript[0].seq, first,
            "keyed replacement retains logical order"
        );
        assert_eq!(
            p.transcript[0].at_us, 4,
            "arrival belongs to the newest accepted replacement"
        );
        assert_eq!(
            p.unread, 0,
            "replacement after drain does not invent a new output"
        );
        assert_eq!(texts(&hub, &k), ["100%", "log line"]);
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
        let utf8 = "界".repeat(5_000);
        hub.publish(&k, text(&utf8), 31).unwrap();
        let p = hub.get(&k).unwrap();
        let visible: Vec<_> = p.visible_transcript(16 * 1024).collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(
            visible[0].text, utf8,
            "whole UTF-8 unit survives newest-fit retention"
        );
        assert!(visible.iter().map(|u| u.text.len()).sum::<usize>() <= 16 * 1024);
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
        let p = hub.get(&k).unwrap();
        assert_eq!(p.unread, 1, "viewer echo never increments output unread");
        assert!(
            p.inputs[0].delivered,
            "poll records delivery without removing unacked input"
        );
        assert_eq!(p.replies[0].text, "yes");
        assert_eq!(p.replies[1].text, "and more");
        assert_eq!(
            texts(&hub, &k),
            ["ask"],
            "echo remains outside agent output"
        );
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
        hub.publish(&k, text("fresh ask"), 700_000_000).unwrap();
        let accepted = hub
            .submit_reply(&k, "fresh answer".into(), 700_000_001)
            .unwrap();
        let before = hub.get(&k).unwrap().replies.len();
        let unread = hub.get(&k).unwrap().unread;
        assert_eq!(
            hub.submit_reply(&k, "界".repeat(4 * 1024), 700_000_002),
            Err(PortalError::TooLarge { limit: 4 * 1024 })
        );
        assert_eq!(
            hub.get(&k).unwrap().replies.len(),
            before,
            "rejected reply has no echo"
        );
        assert_eq!(
            hub.get(&k).unwrap().inputs.len(),
            1,
            "rejected reply has no queue entry"
        );
        assert_eq!(hub.get(&k).unwrap().unread, unread);
        let expires = hub.get(&k).unwrap().inputs[0].expires_us;
        assert_eq!(
            hub.poll_input("a", &[], None, expires - 1).items[0].id,
            accepted
        );
        assert!(
            hub.poll_input("a", &[], None, expires).items.is_empty(),
            "expiry is exact at the boundary"
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
        for publish_hold in [false, true] {
            let mut hub = PortalHub::default();
            let k = key("a", "p");
            if publish_hold {
                hub.publish_with_hold(&k, text("kept"), Some(300_000), 0)
                    .unwrap();
            } else {
                hub.publish(&k, text("kept"), 0).unwrap();
                hub.hold(&k, 300_000, 0).unwrap();
            }
            hub.take_due(0);
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
    }

    #[test]
    fn hold_zero_never_degrades() {
        for publish_hold in [false, true] {
            let mut hub = PortalHub::default();
            let k = key("a", "p");
            if publish_hold {
                hub.publish_with_hold(&k, text("t"), Some(0), 0).unwrap();
            } else {
                hub.publish(&k, text("t"), 0).unwrap();
                hub.hold(&k, 0, 0).unwrap();
            }
            hub.take_due(0);
            assert_eq!(hub.next_deadline(), None);
            assert_eq!(hub.sweep(u64::MAX - 1), []);
            assert_eq!(hub.hold(&key("b", "p"), 0, 0), Err(PortalError::NotHeld));
        }
    }

    #[test]
    fn clear_frees_id_for_fresh_attach() {
        let mut hub = PortalHub::default();
        let k = key("a", "p");
        let first = hub.publish_with_hold(&k, text("old"), None, 0).unwrap();
        assert!(!first.reattached);
        assert_eq!(first.pending_input, 0);
        hub.submit_reply(&k, "r".into(), 0).unwrap();
        let update = hub.publish_with_hold(&k, text("more"), None, 0).unwrap();
        assert!(!update.reattached);
        assert_eq!(update.pending_input, 1);
        hub.clear(&k).unwrap();
        assert_eq!(hub.clear(&k), Err(PortalError::NotHeld));
        assert!(
            !hub.publish_with_hold(&k, text("new"), None, 1)
                .unwrap()
                .reattached
        );
        assert_eq!(texts(&hub, &k), ["new"]);
        assert!(hub.get(&k).unwrap().inputs.is_empty());

        hub.reclaim(&k);
        let again = hub
            .publish_with_hold(&k, text("after reclaim"), Some(0), 2)
            .unwrap();
        assert!(again.reattached);
        assert_eq!(again.pending_input, 0);
        assert_eq!(texts(&hub, &k), ["after reclaim"]);
    }

    /// The owner's Clear reclaims all of one agent's portals, like a viewer
    /// dismiss, and nobody else's (hud-vniri.6).
    #[test]
    fn reclaim_agent_reclaims_only_that_agents_portals() {
        let mut hub = PortalHub::default();
        for (agent, id) in [("a", "p"), ("a", "q"), ("b", "p")] {
            hub.publish(&key(agent, id), text("x"), 0).unwrap();
        }
        assert_eq!(hub.live_counts(), [("a".into(), 2), ("b".into(), 1)].into());
        assert_eq!(hub.reclaim_agent("a"), 2);
        assert_eq!(hub.live_counts(), [("b".into(), 1)].into());
        assert_eq!(hub.reclaim_agent("a"), 0);
        let again = hub
            .publish_with_hold(&key("a", "p"), text("y"), None, 1)
            .unwrap();
        assert!(again.reattached, "the next publish reattaches");
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
        let other = key("b", "p");
        hub.publish(&other, text("other agent"), 300_001).unwrap();
        assert_eq!(
            hub.take_due(300_001),
            [Due {
                key: other.clone(),
                unread: 1
            }]
        );
        assert_eq!(texts(&hub, &other), ["other agent"]);
        assert!(hub.take_due(300_002).is_empty());

        // An admitted local reply must render inside the ordinary 100 ms slot.
        // Timestamps use the Hub's microsecond clock, not elapsed wall time.
        let mut hub = PortalHub::default();
        let k = key("a", "reply");
        hub.publish(&k, text("ready"), 0).unwrap();
        assert_eq!(
            hub.take_due(0),
            [Due {
                key: k.clone(),
                unread: 1
            }]
        );
        hub.submit_reply(&k, "please continue".into(), 1).unwrap();
        let deadline = hub.next_deadline().expect("the reply needs local feedback");
        assert_eq!(deadline.kind, DeadlineKind::Render);
        assert!(deadline.at_us <= 1, "an admitted reply is due immediately");
        assert_eq!(
            hub.take_due(1),
            [Due {
                key: k.clone(),
                unread: 0
            }]
        );
        assert_eq!(hub.take_due(2), [], "local urgency is consumed once");

        hub.publish(&k, text("later output"), 2).unwrap();
        assert_eq!(hub.take_due(2), [], "output alone does not become urgent");
        assert_eq!(hub.take_due(100_000), [], "output keeps the 100 ms pace");
        assert_eq!(
            hub.next_deadline(),
            Some(Deadline {
                at_us: 100_001,
                kind: DeadlineKind::Render
            })
        );
        assert_eq!(hub.take_due(100_001), [Due { key: k, unread: 1 }]);
        assert_eq!(hub.take_due(300_001), [], "no idle feedback frame");
    }

    // Relocated from the existing cooperative adapter flow. Hub publishing is
    // the real implicit attach boundary; there is no external owner token.
    #[test]
    fn portal_attach_publish_reply_poll_detach() {
        let mut hub = PortalHub::default();
        let k = key("projection-daemon", "projection-integration");
        let peer = key("peer", "projection-integration");
        hub.publish(&k, text("assistant: ready over cooperative projection"), 20)
            .unwrap();
        hub.publish(&peer, text("unrelated peer output"), 20)
            .unwrap();
        assert_eq!(
            hub.list("projection-daemon").len(),
            1,
            "implicit attach is agent scoped"
        );
        let id = hub
            .submit_reply(&k, "please summarize the current diff".into(), 30)
            .unwrap();
        let p = hub.get(&k).unwrap();
        assert_eq!(p.inputs.len(), 1);
        assert_eq!(
            p.unread, 1,
            "viewer input does not become unread agent output"
        );
        assert!(
            p.transcript
                .iter()
                .any(|unit| unit.text.contains("ready over cooperative projection"))
        );
        assert_eq!(p.replies[0].text, "please summarize the current diff");
        let poll = hub.poll_input("projection-daemon", &[], None, 40);
        assert_eq!(poll.items.len(), 1);
        assert_eq!(poll.items[0].id, id);
        assert_eq!(poll.items[0].text, "please summarize the current diff");
        assert!(hub.poll_input("peer", &[], None, 40).items.is_empty());
        assert_eq!(
            hub.poll_input("projection-daemon", &[], None, 41).items[0].id,
            id,
            "unacked delivery is stable"
        );
        assert!(
            hub.poll_input("projection-daemon", &[id], None, 42)
                .items
                .is_empty()
        );
        hub.clear(&k).unwrap();
        assert!(hub.list("projection-daemon").is_empty());
        assert!(hub.get(&k).is_none());
        assert_eq!(
            texts(&hub, &peer),
            ["unrelated peer output"],
            "detach cannot affect another agent"
        );
    }
}

#[cfg(test)]
use proptest::prelude::*;

#[cfg(test)]
fn make_snapshot(sequence: u64, gesture_active: bool) -> AdapterGeometrySnapshot {
    AdapterGeometrySnapshot {
        rect: AdapterPortalRect {
            x_px: 10,
            y_px: 20,
            width_px: 400,
            height_px: 300,
        },
        gesture_active,
        sequence,
    }
}

#[cfg(test)]
#[test]
fn coalesce_first_snapshot_is_stored() {
    let mut batch = AdapterGeometryBatch::default();
    assert!(batch.is_empty(), "batch must start empty");

    batch.coalesce(make_snapshot(1, true));

    assert!(!batch.is_empty(), "batch must not be empty after coalesce");
    let latest = batch.latest.expect("latest must be Some after coalesce");
    assert_eq!(latest.sequence, 1, "latest sequence must be 1");
    assert!(latest.gesture_active, "gesture_active must match snapshot");
}

#[cfg(test)]
#[test]
fn coalesce_newer_snapshot_replaces_older() {
    let mut batch = AdapterGeometryBatch::default();
    batch.coalesce(make_snapshot(1, true));
    batch.coalesce(make_snapshot(2, false)); // newer

    let latest = batch.latest.expect("latest must be Some");
    assert_eq!(
        latest.sequence, 2,
        "newer snapshot (seq=2) must replace older (seq=1)"
    );
    assert!(
        !latest.gesture_active,
        "gesture_active from newer snapshot must win"
    );
}

#[cfg(test)]
#[test]
fn coalesce_older_snapshot_does_not_replace_newer() {
    let mut batch = AdapterGeometryBatch::default();
    batch.coalesce(make_snapshot(5, false));
    batch.coalesce(make_snapshot(3, true)); // older — must be dropped

    let latest = batch.latest.expect("latest must be Some");
    assert_eq!(
        latest.sequence, 5,
        "older snapshot (seq=3) must NOT displace newer (seq=5)"
    );
    assert!(
        !latest.gesture_active,
        "gesture_active from older snapshot must be ignored"
    );
}

#[cfg(test)]
#[test]
fn coalesce_equal_sequence_does_not_replace() {
    let mut batch = AdapterGeometryBatch::default();
    batch.coalesce(make_snapshot(7, false));

    // Same sequence with different geometry — first write wins (not strictly
    // required by spec, but the coalescer MUST NOT drop the existing entry
    // for an equal sequence in a way that loses a confirmed snapshot).
    let mut snap_same_seq = make_snapshot(7, true); // gesture_active differs
    snap_same_seq.rect = AdapterPortalRect {
        x_px: 99,
        y_px: 99,
        width_px: 100,
        height_px: 100,
    };
    batch.coalesce(snap_same_seq);

    let latest = batch.latest.expect("latest must be Some");
    assert_eq!(
        latest.sequence, 7,
        "sequence must remain 7 after equal-sequence coalesce attempt"
    );
    assert!(
        !latest.gesture_active,
        "first write (gesture_active=false) must be retained for equal sequence"
    );
}

#[cfg(test)]
proptest! {
    /// For any sequence of (sequence, gesture_active) pairs,
    /// `latest.sequence` must equal the maximum sequence seen.
    #[test]
    fn coalesce_latest_wins_monotone(pairs in proptest::collection::vec((0u64..100u64, proptest::bool::ANY), 1..20usize)) {
        let mut batch = AdapterGeometryBatch::default();
        let mut max_seq = 0u64;
        for (seq, gesture_active) in &pairs {
            if *seq > max_seq { max_seq = *seq; }
            batch.coalesce(make_snapshot(*seq, *gesture_active));
        }
        let latest = batch.latest.expect("batch must be non-empty after coalescing snapshots");
        prop_assert_eq!(
            latest.sequence, max_seq,
            "latest sequence must equal maximum sequence seen"
        );
    }
}
