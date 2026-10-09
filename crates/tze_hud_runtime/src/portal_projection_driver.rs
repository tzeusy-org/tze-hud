//! In-process portal driver: owns the [`PortalHub`] and renders its portals.
//!
//! The hub is the portal state model (keyed by agent and portal id); this
//! driver is the runtime side. On the winit event-loop thread it:
//!
//! 1. Reads the single Hub shared directly with the MCP server.
//! 2. On every `about_to_wait` drain: sweeps the hub (input expiry, idle
//!    degrade, reclaim), then renders each portal [`PortalHub::take_due`]
//!    returns into a scene tile, creating the tile (under its own scene
//!    lease) on first render. Rendering goes through
//!    `ResidentGrpcPortalAdapter::render_batch_with_surface`, fed a
//!    `ProjectedPortalState` built from the hub's portal.
//! 3. Mirrors liveness onto the scene lease: a degraded portal's lease is
//!    orphaned (disconnection badge, content kept, invariant 4) and
//!    reconnected when the agent returns. A reclaimed portal's lease is
//!    revoked, which removes its tile.
//!
//! Follow-tail and head-trim are wired at the render site:
//! `InputProcessor::notify_tile_content_appended` /
//! `notify_head_content_removed` (spec §3.2 / §3.3).
//!
//! `drain` takes `&mut SceneGraph` (the caller holds the scene lock via
//! `try_lock`) and is wrapped in `catch_unwind`: a panic resets the drive
//! state instead of propagating into the event loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tze_hud_config::{resolve_portal_tokens, tokens::DesignTokenMap};
use tze_hud_input::{DraftNotificationBatch, InputProcessor};
pub use tze_hud_mcp::PortalHandle;
pub use tze_hud_projection::hub::{PortalHub, PortalKey};
use tze_hud_projection::{
    AdapterDraftBatch, AdapterDraftCancel, AdapterDraftNotification, AdapterDraftSubmission,
    AdapterGeometryBatch, AdapterGeometrySnapshot, AdapterPortalRect, ContentClassification,
    InputDeliveryState, OutputKind, PortalInputFeedback, PortalInputFeedbackState,
    ProjectedPortalAdapterFamily, ProjectedPortalAttention, ProjectedPortalLayer,
    ProjectedPortalPresentation, ProjectedPortalRuntimeAuthority, ProjectedPortalState,
    ProjectionErrorCode, ProjectionLifecycleState, ProviderKind, TranscriptUnit,
    hub::{DeadlineKind, Portal, PortalError, PortalStatus, Transition, Unit},
    resident_grpc::{
        ResidentGrpcPortalAdapter, ResidentGrpcPortalConfig, portal_visual_tokens_from_part_tokens,
    },
};
use tze_hud_scene::{
    Clock, Rect, SceneGraph,
    types::{LeaseState, SceneId, TileScrollConfig},
};

use crate::idle_efficiency::RuntimeWakeupSource;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum PortalDeadlineFamily {
    ImmediateWork,
    Cadence,
    ActivityCue,
    AgentLiveness,
    PendingInputExpiry,
    LeaseLifecycle,
}

impl PortalDeadlineFamily {
    pub(super) const fn wakeup_source(self) -> RuntimeWakeupSource {
        match self {
            Self::ImmediateWork => RuntimeWakeupSource::SceneChange,
            Self::Cadence | Self::ActivityCue => RuntimeWakeupSource::AnimationDeadline,
            Self::AgentLiveness | Self::PendingInputExpiry | Self::LeaseLifecycle => {
                RuntimeWakeupSource::TtlDeadline
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PortalWakeDeadline {
    pub(super) wall_us: u64,
    pub(super) family: PortalDeadlineFamily,
}

/// Line-height multiplier used by the compositor's text shaper (text.rs).
///
/// `line_height_px = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER`
///
/// Must stay in sync with `tze_hud_compositor::text` — search for `1.4` there.
const PORTAL_LINE_HEIGHT_MULTIPLIER: f32 = 1.4;

/// Namespace of the scene leases and tiles the driver creates for portals.
/// It is distinct from any agent-facing namespace.
pub const PORTAL_DRIVER_NAMESPACE: &str = "tze_hud_portal_driver";

/// Default z-order for portal tiles created by the in-process driver.
const PORTAL_Z_ORDER: u32 = 160;

/// Scene lease TTL for a portal tile (long-lived resident surface).
const PORTAL_LEASE_TTL_MS: u64 = 86_400_000;

/// The wall-clock instant (µs) at which the drive loop should force one repaint
/// so `state`'s agent-activity / streaming-cursor cue quiesces (hud-kbm80), or
/// `None` when the state carries no cue active as of `now_us`.
fn activity_cue_clear_due_us(state: &ProjectedPortalState, now_us: u64) -> Option<u64> {
    tze_hud_projection::resident_grpc::agent_activity_clear_deadline_us(state)
        .filter(|deadline| *deadline >= now_us)
        .map(|deadline| deadline.saturating_add(1))
}

fn adapter_draft_batch_from_runtime(batch: &DraftNotificationBatch) -> AdapterDraftBatch {
    AdapterDraftBatch {
        latest: batch
            .latest
            .as_ref()
            .map(|latest| AdapterDraftNotification {
                text: latest.text.clone(),
                cursor: latest.cursor,
                selection_anchor: latest.selection_anchor,
                at_capacity: latest.at_capacity,
                sequence: latest.sequence,
            }),
        submission: batch
            .submission
            .as_ref()
            .map(|submission| AdapterDraftSubmission {
                text: submission.text.clone(),
                sequence: submission.sequence,
            }),
        cancel: batch.cancel.as_ref().map(|cancel| AdapterDraftCancel {
            sequence: cancel.sequence,
        }),
    }
}

/// Runtime-side state for one portal: its renderer, scene tile and lease,
/// and what the renderer needs beyond the hub's portal.
struct DriveEntry {
    incarnation: Option<u64>,
    adapter: ResidentGrpcPortalAdapter,
    /// Scene lease the portal tile lives under; its orphan/reconnect state
    /// mirrors the portal's liveness.
    scene_lease_id: Option<SceneId>,
    tile_scene_id: Option<SceneId>,
    /// Content height (px) from the last render. A decrease means content was
    /// trimmed from the head (spec §3.3, hud-pkg2g / hud-hkaw2).
    prev_content_height_px: f32,
    /// One-shot instant to repaint so the agent-activity cue quiesces
    /// (hud-kbm80); overwritten by every render.
    activity_cue_clear_due_us: Option<u64>,
    /// Units in the last rendered output batch: the ambient "N unread" count
    /// and the unread divider, kept across repaints that add no output.
    carried_unread: usize,
    /// Newest resize (pointer gesture or hotkey); the durable rendered size.
    latest_geometry: Option<AdapterGeometrySnapshot>,
    /// Resize not yet delivered to the renderer.
    pending_geometry: Option<AdapterGeometryBatch>,
    /// Outcome of the viewer's last composer submit.
    last_input_feedback: Option<PortalInputFeedback>,
}

impl DriveEntry {
    fn new(adapter: ResidentGrpcPortalAdapter) -> Self {
        Self {
            incarnation: None,
            adapter,
            scene_lease_id: None,
            tile_scene_id: None,
            prev_content_height_px: 0.0,
            activity_cue_clear_due_us: None,
            carried_unread: 0,
            latest_geometry: None,
            pending_geometry: None,
            last_input_feedback: None,
        }
    }
}

/// Drive entries keyed by portal, plus scene work deferred to the next drain.
struct InProcessPortalDriveState {
    entries: HashMap<PortalKey, DriveEntry>,
    /// Leases of detached or reclaimed portals; revoking one removes its tile.
    pending_lease_revocations: Vec<SceneId>,
    /// Current resolved design-token overrides (flat key → value strings).
    token_overrides: DesignTokenMap,
}

impl InProcessPortalDriveState {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            pending_lease_revocations: Vec::new(),
            token_overrides: DesignTokenMap::new(),
        }
    }

    fn resolve_visual_tokens(&self) -> tze_hud_projection::resident_grpc::PortalVisualTokens {
        let resolved =
            tze_hud_config::tokens::resolve_tokens(&DesignTokenMap::new(), &self.token_overrides);
        portal_visual_tokens_from_part_tokens(&resolve_portal_tokens(&resolved))
    }

    fn entry(&mut self, key: &PortalKey) -> &mut DriveEntry {
        if !self.entries.contains_key(key) {
            let adapter = ResidentGrpcPortalAdapter::with_tokens(
                ResidentGrpcPortalConfig::new(Vec::new()),
                self.resolve_visual_tokens(),
            );
            self.entries.insert(key.clone(), DriveEntry::new(adapter));
        }
        self.entries.get_mut(key).expect("inserted above")
    }

    /// Drop a portal's entry and queue its surface for removal.
    fn detach(&mut self, key: &PortalKey) {
        if let Some(lease_id) = self.entries.remove(key).and_then(|e| e.scene_lease_id) {
            self.pending_lease_revocations.push(lease_id);
        }
    }

    fn key_for_tile(&self, tile_id: SceneId) -> Option<PortalKey> {
        self.entries
            .iter()
            .find(|(_, entry)| entry.tile_scene_id == Some(tile_id))
            .map(|(key, _)| key.clone())
    }

    fn apply_token_map(&mut self, overrides: DesignTokenMap) {
        self.token_overrides = overrides;
        let tokens = self.resolve_visual_tokens();
        for entry in self.entries.values_mut() {
            entry.adapter.set_visual_tokens(tokens.clone());
        }
    }
}

/// The renderer's view of a hub portal (one step until the renderer reads
/// the hub directly).
fn portal_state(
    key: &PortalKey,
    portal: &Portal,
    entry: &DriveEntry,
    visible_bytes: usize,
) -> ProjectedPortalState {
    let unit = |u: &Unit, output_kind| TranscriptUnit {
        sequence: u.seq,
        output_text: u.text.clone(),
        output_kind,
        content_classification: ContentClassification::default(),
        logical_unit_id: None,
        coalesce_key: u.key.clone(),
        expects_reply: u.expects_reply,
        appended_at_wall_us: u.at_us,
    };
    let visible_transcript: Vec<TranscriptUnit> = portal
        .visible_transcript(visible_bytes)
        .map(|u| unit(u, OutputKind::Assistant))
        .collect();
    let degraded = portal.degraded_since_us.is_some();
    ProjectedPortalState {
        projection_id: key.id.clone(),
        portal_id: format!("text-stream://portal/{}/{}", key.agent, key.id),
        adapter_family: ProjectedPortalAdapterFamily::CooperativeProjection,
        runtime_authority: ProjectedPortalRuntimeAuthority::ResidentSessionLease,
        layer: ProjectedPortalLayer::Content,
        presentation: ProjectedPortalPresentation::Expanded,
        preserve_geometry: true,
        redacted: false,
        connection_degraded: degraded,
        // The portal exists because its agent published: it is connected.
        has_ever_connected: true,
        interaction_enabled: !degraded,
        attention: ProjectedPortalAttention::Ambient,
        provider_kind: Some(ProviderKind::Other),
        display_name: Some(portal.display_name.clone()),
        workspace_hint: None,
        repository_hint: None,
        icon_profile_hint: None,
        hud_target: None,
        lifecycle_state: Some(match portal.status {
            PortalStatus::Attached => ProjectionLifecycleState::Attached,
            PortalStatus::Active => ProjectionLifecycleState::Active,
            PortalStatus::Degraded => ProjectionLifecycleState::Degraded,
            PortalStatus::Detached => ProjectionLifecycleState::Detached,
        }),
        status_text: None,
        visible_transcript_bytes: visible_transcript.iter().map(|u| u.output_text.len()).sum(),
        unread_output_count: Some(entry.carried_unread),
        visible_unread_output_count: Some(entry.carried_unread.min(visible_transcript.len())),
        visible_transcript,
        input_history: portal
            .replies
            .iter()
            .map(|u| unit(u, OutputKind::Viewer))
            .collect(),
        pending_input_count: Some(portal.inputs.len()),
        pending_input_bytes: Some(portal.input_bytes()),
        latest_viewer_delivery_state: portal.inputs.back().map(|i| {
            if i.delivered {
                InputDeliveryState::Delivered
            } else {
                InputDeliveryState::Pending
            }
        }),
        last_input_feedback: entry.last_input_feedback.clone(),
        draft_batch: None,
        geometry_batch: entry.pending_geometry.clone(),
        resized_bounds: entry.latest_geometry.map(|g| g.rect),
    }
}

/// Owns the [`PortalHub`] and drives its portals onto the scene. See the
/// module docs.
pub struct InProcessPortalDriver {
    hub: Arc<Mutex<PortalHub>>,
    drive: InProcessPortalDriveState,
    /// Wall clock for every hub timestamp: op dispatch, sweeps, and wake
    /// deadlines (invariant 9). The windowed runtime uses the system clock;
    /// harnesses share the scene's `TestClock` so portal liveness and lease
    /// grace advance together.
    clock: Arc<dyn Clock>,
    /// Test-only observation alias for the most recently granted portal
    /// lease. Production lifecycle decisions use only the owning DriveEntry.
    #[cfg(test)]
    lease_id: Option<SceneId>,
}

impl InProcessPortalDriver {
    pub fn new() -> Self {
        Self::with_portals(PortalHandle::default())
    }

    pub fn with_portals(portals: PortalHandle) -> Self {
        Self {
            hub: portals.hub,
            drive: InProcessPortalDriveState::new(),
            clock: portals.clock,
            #[cfg(test)]
            lease_id: None,
        }
    }

    pub fn portal_handle(&self) -> PortalHandle {
        PortalHandle {
            hub: Arc::clone(&self.hub),
            clock: Arc::clone(&self.clock),
        }
    }

    fn lock_hub(&self) -> Option<MutexGuard<'_, PortalHub>> {
        match self.hub.lock() {
            Ok(hub) => Some(hub),
            Err(_) => {
                tracing::error!("portal hub poisoned; refusing this driver turn");
                None
            }
        }
    }

    /// Earliest wall-clock instant at which an idle event loop must drive the
    /// portals again: a render, activity-cue quiescence, a liveness
    /// transition, input expiry, or a portal lease's TTL/grace boundary.
    pub(super) fn next_wake_deadline(&self, scene: &SceneGraph) -> Option<PortalWakeDeadline> {
        if self.hub.is_poisoned() {
            return None;
        }
        let now_us = self.now_wall_us();
        // Queued cleanup must win over any stale timed deadline: a missed
        // scene-lock turn needs an immediate retry.
        if self.has_immediate_work(scene) {
            return Some(PortalWakeDeadline {
                wall_us: now_us,
                family: PortalDeadlineFamily::ImmediateWork,
            });
        }
        let hub = self
            .lock_hub()?
            .next_deadline()
            .map(|d| PortalWakeDeadline {
                wall_us: d.at_us,
                family: match d.kind {
                    DeadlineKind::Render => PortalDeadlineFamily::Cadence,
                    DeadlineKind::Liveness => PortalDeadlineFamily::AgentLiveness,
                    DeadlineKind::InputExpiry => PortalDeadlineFamily::PendingInputExpiry,
                },
            });
        let activity = self
            .drive
            .entries
            .values()
            .filter_map(|entry| entry.activity_cue_clear_due_us)
            .min()
            .map(|wall_us| PortalWakeDeadline {
                wall_us,
                family: PortalDeadlineFamily::ActivityCue,
            });
        let lease = self
            .drive
            .entries
            .values()
            .filter_map(|entry| scene.leases.get(&entry.scene_lease_id?))
            .filter_map(|lease| {
                let ttl =
                    (lease.ttl_ms > 0).then(|| lease.granted_at_ms.saturating_add(lease.ttl_ms));
                match lease.state {
                    LeaseState::Active | LeaseState::Requested => ttl,
                    LeaseState::Orphaned => {
                        let grace = lease
                            .disconnected_at_ms
                            .map(|at| at.saturating_add(lease.grace_period_ms));
                        ttl.into_iter().chain(grace).min()
                    }
                    LeaseState::Suspended => lease
                        .suspended_at_ms
                        .map(|at| at.saturating_add(SceneGraph::DEFAULT_MAX_SUSPENSION_MS)),
                    LeaseState::Denied
                    | LeaseState::Revoked
                    | LeaseState::Expired
                    | LeaseState::Released => None,
                }
            })
            .min()
            .map(|ms| PortalWakeDeadline {
                wall_us: ms.saturating_mul(1_000),
                family: PortalDeadlineFamily::LeaseLifecycle,
            });
        [hub, activity, lease]
            .into_iter()
            .flatten()
            .min_by_key(|deadline| (deadline.wall_us, deadline.family))
    }

    /// Scene work that must run on the next drain even with no timed
    /// deadline: queued lease revocations, or a portal whose lease the scene
    /// already reaped.
    fn has_immediate_work(&self, scene: &SceneGraph) -> bool {
        !self.drive.pending_lease_revocations.is_empty()
            || self.lock_hub().is_some_and(|hub| {
                self.drive
                    .entries
                    .iter()
                    .any(|(key, entry)| hub.incarnation(key) != entry.incarnation)
            })
            || self.drive.entries.values().any(|entry| {
                entry.scene_lease_id.is_some_and(|lease_id| {
                    scene
                        .leases
                        .get(&lease_id)
                        .is_none_or(|lease| lease.state.is_terminal())
                })
            })
    }

    /// Viewer dismiss of a tile from the hover close button.
    ///
    /// If the tile is a portal, the hub reclaims it first, so the agent's
    /// next `hud_publish` attaches a fresh portal, `hud_hold` is `NOT_HELD`,
    /// and `hud_clear` is an ok no-op. The tile's lease is then reclaimed
    /// like any other viewer dismiss ([`crate::shell::dismiss_tile`]). Local
    /// and immediate; nothing waits on the agent.
    pub fn viewer_dismiss_tile(
        &mut self,
        scene: &mut SceneGraph,
        tile_id: SceneId,
    ) -> crate::shell::DismissTileResult {
        if let Some(key) = self.drive.key_for_tile(tile_id) {
            let Some(mut hub) = self.lock_hub() else {
                // A failed agent service never vetoes the human's override.
                return crate::shell::dismiss_tile(scene, tile_id);
            };
            hub.reclaim(&key);
            drop(hub);
            self.drive.entries.remove(&key);
            tracing::info!(portal = %key.id, "portal: viewer dismissed surface — reclaimed");
        }
        crate::shell::dismiss_tile(scene, tile_id)
    }

    /// Replace the driver's wall clock. Harnesses pass the scene's
    /// `TestClock` so portal liveness and lease grace share one time source.
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    /// Current wall-clock time (µs since the Unix epoch) from the driver's clock.
    pub fn now_wall_us(&self) -> u64 {
        self.clock.now_us()
    }

    /// Record a resize of a portal tile (hotkey or pointer gesture, §6b.4,
    /// hud-npq6g) so its next render uses the new bounds. Returns `false` if
    /// no portal owns `tile_id` or the snapshot is not newer than the last.
    pub fn push_geometry_snapshot_for_tile(
        &mut self,
        tile_id: SceneId,
        snapshot: tze_hud_input::GeometrySnapshot,
    ) -> bool {
        let Some(key) = self.drive.key_for_tile(tile_id) else {
            return false;
        };
        let entry = self.drive.entry(&key);
        let snapshot = AdapterGeometrySnapshot {
            rect: AdapterPortalRect::from_f32(
                snapshot.rect.x,
                snapshot.rect.y,
                snapshot.rect.width,
                snapshot.rect.height,
            ),
            gesture_active: snapshot.gesture_active,
            sequence: snapshot.sequence,
        };
        if entry
            .latest_geometry
            .is_some_and(|latest| snapshot.sequence <= latest.sequence)
        {
            return false;
        }
        entry.latest_geometry = Some(snapshot);
        entry
            .pending_geometry
            .get_or_insert_with(AdapterGeometryBatch::default)
            .coalesce(snapshot);
        true
    }

    /// Route a focused portal composer batch to the portal that owns the tile.
    ///
    /// The adapter consumes the draft batch; a submission is queued in the
    /// hub as input for the agent's `hud_input` (and echoed in the reply
    /// band). Returns `None` when no portal owns `tile_id` or the batch has
    /// no submission.
    pub fn submit_composer_batch_for_tile(
        &mut self,
        tile_id: SceneId,
        batch: &DraftNotificationBatch,
        submitted_at_wall_us: u64,
    ) -> Option<PortalInputFeedback> {
        let key = self.drive.key_for_tile(tile_id)?;
        let adapter_batch = adapter_draft_batch_from_runtime(batch);
        let text = adapter_batch.submission.as_ref()?.text.clone();
        let mut hub = self.lock_hub()?;
        let result = hub.submit_reply(&key, text, submitted_at_wall_us);
        let (pending_input_count, pending_input_bytes) = hub
            .get(&key)
            .map(|p| (p.inputs.len(), p.input_bytes()))
            .unwrap_or_default();
        drop(hub);
        self.drive
            .entry(&key)
            .adapter
            .consume_draft_batch(&adapter_batch);
        let feedback = match result {
            Ok(input_id) => PortalInputFeedback {
                projection_id: key.id.clone(),
                input_id,
                feedback_state: PortalInputFeedbackState::Accepted,
                error_code: None,
                pending_input_count,
                pending_input_bytes,
                status_summary: "portal input accepted".to_string(),
            },
            Err(error) => PortalInputFeedback {
                projection_id: key.id.clone(),
                input_id: String::new(),
                feedback_state: PortalInputFeedbackState::Rejected,
                error_code: Some(match error {
                    PortalError::TooLarge { .. } => ProjectionErrorCode::ProjectionInputTooLarge,
                    PortalError::QueueFull => ProjectionErrorCode::ProjectionInputQueueFull,
                    PortalError::NotHeld => ProjectionErrorCode::ProjectionNotFound,
                    PortalError::InvalidArgument => ProjectionErrorCode::ProjectionInvalidArgument,
                }),
                pending_input_count,
                pending_input_bytes,
                status_summary: "portal input rejected".to_string(),
            },
        };
        self.drive.entry(&key).last_input_feedback = Some(feedback.clone());
        Some(feedback)
    }

    /// Apply a new design-token override map, propagating to all live adapters.
    pub fn apply_token_map(&mut self, overrides: DesignTokenMap) {
        self.drive.apply_token_map(overrides);
    }

    /// Test support: queue portal input with a fixed id (the token-footprint
    /// calibration's canonical input).
    #[doc(hidden)]
    pub fn inject_input(&mut self, key: &PortalKey, id: &str, text: &str) -> bool {
        self.lock_hub()
            .is_some_and(|mut hub| hub.inject_input(key, id, text).is_ok())
    }

    #[cfg(test)]
    pub(crate) fn hub_mut(&self) -> MutexGuard<'_, PortalHub> {
        self.hub.lock().expect("test hub")
    }

    /// Sweep the hub and render due portals onto the scene.
    ///
    /// Called from `about_to_wait` after the composer-draft flush, with the
    /// scene lock already taken (`try_lock`). `tab_id` is the active tab; a
    /// new portal tile goes there, or into the first tab (activating it), or
    /// into a fresh "Main" tab.
    pub fn drain(
        &mut self,
        scene: &mut SceneGraph,
        input_processor: &mut InputProcessor,
        tab_id: Option<SceneId>,
    ) {
        if self.hub.is_poisoned() {
            tracing::error!("portal hub poisoned; refusing drain without resetting drive state");
            return;
        }
        let now_us = self.now_wall_us();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.drain_inner(scene, input_processor, tab_id, now_us)
        }));
        if let Err(payload) = result {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic payload".to_string()
            };
            tracing::error!(
                error = %msg,
                "portal projection driver drain panicked — drive state reset"
            );
            if !self.hub.is_poisoned() {
                self.drive = InProcessPortalDriveState::new();
            }
        }
    }

    /// The tab a new portal tile goes into when none is active: the lowest
    /// `display_order` tab, activated only after the tile is created
    /// (hud-zccuf), so a failed create leaves `active_tab` alone.
    fn find_portal_host_tab(scene: &SceneGraph) -> Option<SceneId> {
        scene
            .tabs
            .values()
            .min_by_key(|t| t.display_order)
            .map(|t| t.id)
    }

    /// `drain` at a caller-supplied wall-clock instant, so tests are
    /// deterministic.
    fn drain_inner(
        &mut self,
        scene: &mut SceneGraph,
        input_processor: &mut InputProcessor,
        tab_id: Option<SceneId>,
        now_us: u64,
    ) {
        let (stale, transitions, due) = {
            let Some(mut hub) = self.lock_hub() else {
                return;
            };
            let stale: Vec<_> = self
                .drive
                .entries
                .iter()
                .filter(|(key, entry)| hub.incarnation(key) != entry.incarnation)
                .map(|(key, _)| key.clone())
                .collect();
            (stale, hub.sweep(now_us), hub.take_due(now_us))
        };
        for key in stale {
            self.drive.detach(&key);
        }
        for transition in transitions {
            match transition {
                Transition::Degraded(key) => {
                    tracing::info!(portal = %key.id, "portal: agent idle — degraded");
                    if let Some(entry) = self.drive.entries.get_mut(&key) {
                        entry.activity_cue_clear_due_us = None;
                    }
                }
                Transition::Reclaimed(key) => {
                    tracing::info!(portal = %key.id, "portal: reclaimed after degraded window");
                    self.drive.detach(&key);
                }
            }
        }
        self.revoke_pending_leases(scene);
        // A returning agent's orphaned lease must be Active again before
        // rendering: a render under an orphaned lease is rejected.
        self.reconnect_recovered_leases(scene);

        for due in due {
            if due.unread > 0 {
                self.drive.entry(&due.key).carried_unread = due.unread;
            }
            self.render(&due.key, scene, input_processor, tab_id, now_us);
        }

        // Activity-cue quiesce (hud-kbm80): the "⋯ writing" cue derives from
        // the newest unit's age, so an idle portal needs one repaint once it
        // has aged out.
        let quiesce: Vec<PortalKey> = self
            .drive
            .entries
            .iter()
            .filter(|(_, e)| {
                e.tile_scene_id.is_some()
                    && e.activity_cue_clear_due_us.is_some_and(|due| now_us >= due)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in quiesce {
            self.render(&key, scene, input_processor, tab_id, now_us);
        }

        // Orphan after rendering, so the degraded repaint lands first.
        self.orphan_degraded_leases(scene);
        self.forget_lost_surfaces(scene);
    }

    /// Render one portal: create its tile on first render, then paint and
    /// update scroll accounting.
    fn render(
        &mut self,
        key: &PortalKey,
        scene: &mut SceneGraph,
        input_processor: &mut InputProcessor,
        tab_id: Option<SceneId>,
        now_us: u64,
    ) {
        let incarnation = {
            let Some(hub) = self.lock_hub() else {
                return;
            };
            let Some(incarnation) = hub.incarnation(key) else {
                return;
            };
            incarnation
        };
        if self
            .drive
            .entries
            .get(key)
            .is_some_and(|entry| entry.incarnation != Some(incarnation))
        {
            self.drive.detach(key);
            self.revoke_pending_leases(scene);
        }
        self.drive.entry(key).incarnation = Some(incarnation);
        let created = self.drive.entry(key).tile_scene_id.is_none();
        if created && !self.create_tile(key, scene, tab_id) {
            return;
        }
        let state = {
            let Some(hub) = self.lock_hub() else {
                return;
            };
            // A direct clear or replacement may race tile creation. Leave its
            // stale lease as immediate work rather than render an old snapshot.
            if hub.incarnation(key) != Some(incarnation) {
                return;
            }
            let portal = hub.get(key).expect("incarnation checked above");
            portal_state(
                key,
                portal,
                &self.drive.entries[key],
                hub.limits().visible_bytes,
            )
        };
        let entry = self
            .drive
            .entries
            .get_mut(key)
            .expect("entry ensured above");
        let Some(tile_id) = entry.tile_scene_id else {
            return;
        };
        entry.pending_geometry = None;
        entry.activity_cue_clear_due_us = activity_cue_clear_due_us(&state, now_us);
        match entry.adapter.render_batch_with_surface(&state, now_us) {
            Ok(batch) => tze_hud_protocol::convert::apply_portal_render_batch_to_scene(
                scene,
                tile_id,
                PORTAL_DRIVER_NAMESPACE,
                &batch,
            ),
            Err(e) => {
                tracing::warn!(portal = %key.id, error = ?e, "portal render failed");
            }
        }
        if created {
            return;
        }

        // Scroll accounting (spec §3.2 / §3.3). The content height counts
        // every rendered line: the OUTPUT transcript plus, when expanded, the
        // INPUT band (label + replies + trailing blank) painted into the same
        // markdown.
        let line_height_px =
            entry.adapter.visual_tokens().transcript_font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER;
        let lines = |units: &[TranscriptUnit]| -> usize {
            units
                .iter()
                .map(|u| u.output_text.lines().count().max(1))
                .sum()
        };
        let mut total_lines = lines(&state.visible_transcript);
        if !state.input_history.is_empty() {
            total_lines += 2 + lines(&state.input_history);
        }
        let new_content_height_px = total_lines.max(1) as f32 * line_height_px;
        // The live resize snapshot, then the durable resized bounds, then the
        // adapter's configured height (hud-v4k1h).
        let viewport_height_px = state
            .geometry_batch
            .as_ref()
            .and_then(|gb| gb.latest)
            .map(|snap| snap.rect.height_px as f32)
            .or_else(|| state.resized_bounds.map(|r| r.height_px as f32))
            .unwrap_or_else(|| entry.adapter.config_viewport_height(state.presentation));
        // Any height decrease is a head trim (retention or the visible
        // window), notified before the append so the scrolled-back offset
        // stays put (hud-66i1s).
        if new_content_height_px < entry.prev_content_height_px {
            input_processor.notify_head_content_removed(
                tile_id,
                entry.prev_content_height_px - new_content_height_px,
            );
        }
        entry.prev_content_height_px = new_content_height_px;
        input_processor.notify_tile_content_appended(
            tile_id,
            new_content_height_px,
            viewport_height_px,
            line_height_px,
            scene,
        );
        // The jump-to-latest pill's badge (hud-g1ena.3).
        scene.set_tile_unread_count(tile_id, state.unread_output_count.unwrap_or(0));
    }

    /// Create a portal's tile under its own scene lease. A portal needs a
    /// host tab even when none is active (hud-obw3q); tab activation waits
    /// until the tile exists (hud-zccuf). On failure the portal stays in the
    /// hub and is retried on its next change.
    fn create_tile(
        &mut self,
        key: &PortalKey,
        scene: &mut SceneGraph,
        tab_id: Option<SceneId>,
    ) -> bool {
        let (host_tab, activate) = match tab_id {
            Some(tab) => (Some(tab), None),
            None => match Self::find_portal_host_tab(scene) {
                Some(tab) => (Some(tab), Some(tab)),
                // `create_tab` auto-activates when no tab is active.
                None => (scene.create_tab("Main", 0).ok(), None),
            },
        };
        let Some(host_tab) = host_tab else {
            tracing::warn!(portal = %key.id, "portal: no tab for a new portal tile");
            return false;
        };
        let Some(lease_id) = self.ensure_portal_lease(key, scene) else {
            tracing::warn!(portal = %key.id, "portal: no lease for a new portal tile");
            return false;
        };
        let entry = self.drive.entry(key);
        let viewport_h = entry
            .adapter
            .config_viewport_height(ProjectedPortalPresentation::Expanded);
        let bounds = Rect::new(0.0, 0.0, 720.0, viewport_h);
        match scene.create_tile(
            host_tab,
            PORTAL_DRIVER_NAMESPACE,
            lease_id,
            bounds,
            PORTAL_Z_ORDER,
        ) {
            Ok(tile_id) => {
                let _ = scene.register_tile_scroll_config(
                    tile_id,
                    TileScrollConfig {
                        scrollable_x: false,
                        scrollable_y: true,
                        content_width: None,
                        content_height: None,
                    },
                );
                entry
                    .adapter
                    .record_created_tile(tile_id.to_bytes_le().to_vec());
                entry.tile_scene_id = Some(tile_id);
                if let Some(tab) = activate {
                    let _ = scene.switch_active_tab(tab);
                }
                true
            }
            Err(e) => {
                tracing::warn!(portal = %key.id, error = ?e, "portal: tile creation failed");
                false
            }
        }
    }

    /// The portal's scene lease, granting one if it has none or its old one
    /// is terminal (a portal re-attached after reclaim starts fresh). Never
    /// replaces an orphaned lease: that grace window belongs to the portal.
    fn ensure_portal_lease(&mut self, key: &PortalKey, scene: &mut SceneGraph) -> Option<SceneId> {
        let entry = self.drive.entry(key);
        if let Some(lease_id) = entry.scene_lease_id {
            if scene.lease_is_active(&lease_id) {
                return Some(lease_id);
            }
            if scene.lease_is_orphaned(&lease_id) {
                return None;
            }
        }
        let lease_id = scene.grant_lease(PORTAL_DRIVER_NAMESPACE, PORTAL_LEASE_TTL_MS);
        entry.scene_lease_id = Some(lease_id);
        #[cfg(test)]
        {
            self.lease_id = Some(lease_id);
        }
        Some(lease_id)
    }

    #[cfg(test)]
    fn degraded(&self, key: &PortalKey) -> bool {
        self.lock_hub()
            .is_some_and(|hub| hub.get(key).is_some_and(|p| p.degraded_since_us.is_some()))
    }

    /// Resume within grace (invariant 4): a portal whose agent came back gets
    /// its orphaned lease reconnected, keeping the same tile.
    fn reconnect_recovered_leases(&mut self, scene: &mut SceneGraph) {
        let recovered = {
            let Some(hub) = self.lock_hub() else {
                return;
            };
            self.drive
                .entries
                .iter()
                .filter_map(|(key, entry)| {
                    let portal = hub.get(key)?;
                    if hub.incarnation(key) == entry.incarnation
                        && portal.degraded_since_us.is_none()
                    {
                        Some((key.clone(), entry.scene_lease_id?))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        let now_ms = scene.now_millis();
        for (key, lease_id) in recovered {
            if !scene.lease_is_orphaned(&lease_id) {
                continue;
            }
            match scene.reconnect_lease(&lease_id, now_ms) {
                Ok(()) => tracing::info!(portal = %key.id, "portal: lease reconnected — resumed"),
                Err(error) => {
                    tracing::warn!(portal = %key.id, ?error, "portal: lease reconnect failed")
                }
            }
        }
    }

    /// A degraded portal's lease keeps its content until reconnect or reclaim.
    fn orphan_degraded_leases(&mut self, scene: &mut SceneGraph) {
        let degraded = {
            let Some(hub) = self.lock_hub() else {
                return;
            };
            self.drive
                .entries
                .iter()
                .filter_map(|(key, entry)| {
                    let portal = hub.get(key)?;
                    if hub.incarnation(key) == entry.incarnation
                        && portal.degraded_since_us.is_some()
                    {
                        Some((key.clone(), entry.scene_lease_id?))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        let now_ms = scene.now_millis();
        for (key, lease_id) in degraded {
            if !scene.lease_is_active(&lease_id) {
                continue;
            }
            if let Err(error) = scene.disconnect_lease(&lease_id, now_ms) {
                tracing::warn!(portal = %key.id, ?error, "portal: lease orphan failed");
            }
        }
    }

    /// A portal whose lease the scene already ended (orphan grace or TTL
    /// expiry swept elsewhere) has lost its surface: reclaim it in the hub.
    fn forget_lost_surfaces(&mut self, scene: &SceneGraph) {
        let lost: Vec<PortalKey> = self
            .drive
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.scene_lease_id.is_some_and(|lease_id| {
                    scene
                        .leases
                        .get(&lease_id)
                        .is_none_or(|lease| lease.state.is_terminal())
                })
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in lost {
            tracing::info!(portal = %key.id, "portal: surface lease ended — reclaimed");
            let Some(mut hub) = self.lock_hub() else {
                return;
            };
            // Do not reclaim a new attachment because the old surface expired.
            if hub.incarnation(&key) == self.drive.entries[&key].incarnation {
                hub.reclaim(&key);
            }
            drop(hub);
            self.drive.entries.remove(&key);
        }
    }

    fn revoke_pending_leases(&mut self, scene: &mut SceneGraph) {
        for lease_id in self.drive.pending_lease_revocations.drain(..) {
            if let Err(error) = scene.revoke_lease(lease_id) {
                tracing::debug!(?lease_id, ?error, "portal: lease already gone");
            }
        }
    }
}

impl Default for InProcessPortalDriver {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_input::{DraftSubmission, GeometrySnapshot, PortalRect};
    use tze_hud_projection::hub::PortalLimits;
    use tze_hud_scene::{NodeData, TestClock};

    /// The header marker line the resident adapter paints while the agent is
    /// actively appending (mirrors `resident_grpc::PORTAL_ACTIVITY_MARKER_LINE`).
    const PORTAL_ACTIVITY_MARKER_TEXT: &str = "⋯ writing";
    const AGENT: &str = "agent";
    /// One hub render interval: drains this far apart always render.
    const FRAME_US: u64 = 100_000;

    fn key(id: &str) -> PortalKey {
        PortalKey::new(AGENT, id)
    }

    /// Direct `hud_publish` state transition at `now_us`.
    fn publish(driver: &mut InProcessPortalDriver, id: &str, text: &str, now_us: u64) {
        publish_for(driver, &key(id), text, now_us);
    }

    fn publish_for(driver: &InProcessPortalDriver, key: &PortalKey, text: &str, now_us: u64) {
        driver
            .hub_mut()
            .publish(
                key,
                tze_hud_projection::hub::Publish {
                    display_name: None,
                    text: Some(text.into()),
                    key: None,
                    expects_reply: false,
                    status: None,
                },
                now_us,
            )
            .expect("publish accepted");
    }

    fn hold(
        driver: &mut InProcessPortalDriver,
        agent: &str,
        id: &str,
        ttl_ms: u64,
        now_us: u64,
    ) -> Result<(), PortalError> {
        driver
            .hub_mut()
            .hold(&PortalKey::new(agent, id), ttl_ms, now_us)
    }

    fn scene() -> (SceneGraph, SceneId, InputProcessor) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab = scene.create_tab("Main", 0).unwrap();
        (scene, tab, InputProcessor::new())
    }

    /// Driver and scene sharing one `TestClock`, as the harnesses run them.
    fn clocked() -> (
        InProcessPortalDriver,
        SceneGraph,
        SceneId,
        InputProcessor,
        TestClock,
    ) {
        let clock = TestClock::new(1_000);
        let mut driver = InProcessPortalDriver::new();
        driver.set_clock(Arc::new(clock.clone()));
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab = scene.create_tab("Main", 0).unwrap();
        (driver, scene, tab, InputProcessor::new(), clock)
    }

    fn tile(driver: &InProcessPortalDriver, id: &str) -> SceneId {
        driver.drive.entries[&key(id)]
            .tile_scene_id
            .expect("drain created the portal tile")
    }

    fn lease(driver: &InProcessPortalDriver, id: &str) -> SceneId {
        driver.drive.entries[&key(id)]
            .scene_lease_id
            .expect("portal lease")
    }

    /// Shrink the portal to a one-line viewport (tile bounds plus the resize
    /// snapshot the window layer pushes). Returns the line height.
    fn one_line_viewport(
        driver: &mut InProcessPortalDriver,
        scene: &mut SceneGraph,
        id: &str,
    ) -> f32 {
        let tile_id = tile(driver, id);
        let line_h = driver.drive.entries[&key(id)]
            .adapter
            .visual_tokens()
            .transcript_font_size_px
            * PORTAL_LINE_HEIGHT_MULTIPLIER;
        let viewport_h = line_h.ceil();
        let _ = scene.update_tile_bounds(
            tile_id,
            Rect::new(0.0, 0.0, 600.0, viewport_h),
            PORTAL_DRIVER_NAMESPACE,
        );
        let snapshot = GeometrySnapshot {
            portal_id_hash: 0,
            rect: PortalRect {
                x: 0.0,
                y: 0.0,
                width: 600.0,
                height: viewport_h,
            },
            gesture_active: false,
            sequence: 1,
        };
        assert!(
            driver.push_geometry_snapshot_for_tile(tile_id, snapshot),
            "a portal tile accepts its resize snapshot"
        );
        line_h
    }

    fn submit(
        driver: &mut InProcessPortalDriver,
        tile_id: SceneId,
        text: &str,
        now_us: u64,
    ) -> PortalInputFeedback {
        let mut batch = DraftNotificationBatch::new();
        batch.record_submission(DraftSubmission {
            text: text.to_string(),
            sequence: 1,
        });
        driver
            .submit_composer_batch_for_tile(tile_id, &batch, now_us)
            .expect("the tile belongs to a portal")
    }

    /// All painted text in the portal subtree.
    fn tile_markdown(scene: &SceneGraph, tile_id: SceneId) -> String {
        fn collect(scene: &SceneGraph, node_id: SceneId, output: &mut String) {
            let node = scene.nodes.get(&node_id).expect("node id must resolve");
            if let NodeData::TextMarkdown(text) = &node.data {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&text.content);
            }
            for child in &node.children {
                collect(scene, *child, output);
            }
        }
        let root_id = scene.tiles[&tile_id]
            .root_node
            .expect("portal tile must have a painted root node");
        let mut output = String::new();
        collect(scene, root_id, &mut output);
        assert!(!output.is_empty(), "portal subtree must paint text content");
        output
    }

    #[test]
    fn portal_composer_submission_enters_pending_input_queue_and_can_be_acknowledged() {
        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        publish(&mut driver, "p", "assistant is ready", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200);
        let tile_id = tile(&driver, "p");

        let feedback = submit(
            &mut driver,
            tile_id,
            "please summarize the current diff",
            1_000,
        );
        assert_eq!(feedback.feedback_state, PortalInputFeedbackState::Accepted);
        assert_eq!(feedback.pending_input_count, 1);

        let poll = |driver: &mut InProcessPortalDriver, ack: Vec<String>, now_us| {
            driver.hub_mut().poll_input(AGENT, &ack, None, now_us)
        };
        let batch = poll(&mut driver, Vec::new(), 1_100);
        assert_eq!(batch.items.len(), 1);
        assert_eq!(batch.items[0].text, "please summarize the current diff");
        assert_eq!(batch.items[0].portal, "p");
        assert!(
            poll(&mut driver, vec![batch.items[0].id.clone()], 1_200)
                .items
                .is_empty()
        );

        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200 + FRAME_US);
        let content = tile_markdown(&scene, tile_id);
        assert!(content.contains("pending HUD input: 0"), "{content}");
        assert!(
            content.contains("please summarize"),
            "the reply is echoed: {content}"
        );
    }

    /// The render site wires follow-tail (spec §3.2), the resize snapshot
    /// (§6b.4, hud-npq6g), and the jump-to-latest unread badge (hud-g1ena.3).
    #[test]
    fn drain_render_portal_notify_tile_content_appended_wiring() {
        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        publish(&mut driver, "p", "line-0", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200);
        let tile_id = tile(&driver, "p");
        assert!(scene.tile_scroll_config(tile_id).is_some());

        // Without the snapshot the drain would size against the configured
        // ~360 px viewport, which ten lines don't overflow.
        one_line_viewport(&mut driver, &mut scene, "p");
        for i in 1..=9 {
            publish(&mut driver, "p", &format!("line-{i}"), 1_000 + i);
        }
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200 + FRAME_US);

        assert!(scene.tile_follow_tail_at_tail(tile_id));
        let (_, scroll_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(scroll_y > 0.0, "follow-tail advanced to the tail");
        assert_eq!(
            scene.tile_unread_count(tile_id),
            9,
            "the drained batch's unread count"
        );
        let entry = &driver.drive.entries[&key("p")];
        assert!(
            entry.pending_geometry.is_none(),
            "the resize is delivered once"
        );
        assert!(entry.latest_geometry.is_some(), "the resized size persists");
    }

    /// The content height counts the INPUT band (viewer replies), not just
    /// the OUTPUT transcript (codex P2, hud-y4pzu).
    #[test]
    fn drain_render_portal_height_accounting_includes_input_history() {
        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        publish(&mut driver, "p", "line-0", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200);
        let tile_id = tile(&driver, "p");
        let line_h = one_line_viewport(&mut driver, &mut scene, "p");
        for i in 0..10 {
            let feedback = submit(&mut driver, tile_id, &format!("viewer reply {i}"), 300 + i);
            assert_eq!(feedback.feedback_state, PortalInputFeedbackState::Accepted);
        }
        publish(&mut driver, "p", "line-1", 1_000);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200 + FRAME_US);
        let (_, offset_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            offset_y > 5.0 * line_h,
            "offset {offset_y} must cover the ten-reply INPUT band (line {line_h})"
        );
    }

    /// spec §3.3 — `notify_tile_content_appended` on a scrolled-back tile must
    /// NOT change the scroll offset (InputProcessor enforces this; driver just calls it).
    #[test]
    fn scrolled_back_tile_offset_is_stable_on_append() {
        use tze_hud_input::ScrollEvent;

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("portal-agent", 60_000);
        let viewport_h = 200.0_f32;
        let tile_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(0.0, 0.0, 600.0, viewport_h),
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(
                tile_id,
                TileScrollConfig {
                    scrollable_x: false,
                    scrollable_y: true,
                    content_width: None,
                    content_height: None,
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();

        // Prime at-tail with large content.
        let line_h = 18.2_f32;
        let large_content = viewport_h * 5.0;
        processor.notify_tile_content_appended(
            tile_id,
            large_content,
            viewport_h,
            line_h,
            &mut scene,
        );

        // User scrolls back.  The tile occupies (0,0)→(600,200); use (300,100)
        // as the event coordinate so hit-testing resolves to this tile.
        let scroll_ev = ScrollEvent {
            x: 300.0,
            y: 100.0,
            delta_x: 0.0,
            delta_y: -50.0,
        };
        processor.process_scroll_event(&scroll_ev, &mut scene);

        let (_, pre_y) = scene.tile_scroll_offset_local(tile_id);

        // spec §3.3: another append must NOT change the scrolled-back position.
        let changed = processor.notify_tile_content_appended(
            tile_id,
            large_content + line_h,
            viewport_h,
            line_h,
            &mut scene,
        );

        let (_, post_y) = scene.tile_scroll_offset_local(tile_id);

        assert!(
            !changed,
            "spec §3.3: scrolled-back tile must not advance after append"
        );
        assert!(
            (post_y - pre_y).abs() < f32::EPSILON,
            "spec §3.3: scroll offset must be unchanged; pre={pre_y} post={post_y}"
        );
    }

    #[test]
    fn apply_token_map_propagates_to_drive_state() {
        let font = |driver: &InProcessPortalDriver, id: &str| {
            driver.drive.entries[&key(id)]
                .adapter
                .visual_tokens()
                .transcript_font_size_px
        };
        let tokens = |size: &str| {
            let mut map = DesignTokenMap::new();
            map.insert("portal.transcript.font_size".to_string(), size.to_string());
            map
        };
        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        driver.apply_token_map(tokens("32"));
        publish(&mut driver, "pre", "x", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200);
        assert_eq!(
            font(&driver, "pre"),
            32.0,
            "a new portal takes the current tokens"
        );

        driver.apply_token_map(tokens("48"));
        assert_eq!(
            font(&driver, "pre"),
            48.0,
            "a live portal picks up new tokens"
        );
    }

    #[test]
    fn drain_paints_published_transcript_onto_tile() {
        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        publish(&mut driver, "p", "UNIQUE-TRANSCRIPT-MARKER-42", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200);
        let tile_id = tile(&driver, "p");
        assert!(
            tile_markdown(&scene, tile_id).contains("UNIQUE-TRANSCRIPT-MARKER-42"),
            "the create drain paints the first publish (the grey-tile bug, hud-utbiy)"
        );

        publish(&mut driver, "p", "SECOND-TRANSCRIPT-MARKER-99", 300);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200 + FRAME_US);
        assert!(tile_markdown(&scene, tile_id).contains("SECOND-TRANSCRIPT-MARKER-99"));
        assert_eq!(scene.tile_count(), 1);
    }

    #[test]
    fn drain_declares_first_class_portal_surface_on_tile() {
        use tze_hud_scene::types::{PortalDisplayState, PortalPartKind};

        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        publish(&mut driver, "p", "hello surface", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200);
        let tile_id = tile(&driver, "p");

        let surface = scene
            .portal_surface(tile_id)
            .expect("drain must declare a first-class portal surface on the tile");
        for expected in [
            PortalPartKind::Frame,
            PortalPartKind::Header,
            PortalPartKind::Composer,
            PortalPartKind::Transcript,
            PortalPartKind::Divider,
        ] {
            assert!(
                surface.parts.iter().any(|part| part.kind == expected),
                "native expanded surface must declare {expected:?}"
            );
        }
        assert_eq!(surface.display_state, PortalDisplayState::Expanded);

        let root_id = scene.tiles[&tile_id].root_node.expect("root painted");
        let root = &scene.nodes[&root_id];
        assert!(
            matches!(root.data, NodeData::SolidColor(_)),
            "production drain must materialize the native frame root"
        );
        let child_text: Vec<&str> = root
            .children
            .iter()
            .filter_map(|id| match &scene.nodes.get(id)?.data {
                NodeData::TextMarkdown(text) => Some(text.content.as_str()),
                _ => None,
            })
            .collect();
        assert!(child_text.contains(&"INPUT"));
        assert!(child_text.contains(&"OUTPUT"));
        assert!(child_text.iter().any(|text| text.contains("hello surface")));
    }

    /// The ambient "N unread" count and the in-transcript divider come from
    /// the drained batch (hud-meqet, hud-n95zc) and survive a degraded repaint
    /// that adds no output (hud-lylbz).
    #[test]
    fn drain_then_render_surfaces_live_unread_count() {
        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        for i in 0..3 {
            publish(&mut driver, "p", &format!("line-{i}"), 100 + i);
        }
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200);
        let tile_id = tile(&driver, "p");
        let content = tile_markdown(&scene, tile_id);
        assert!(content.contains("3 unread"), "{content}");
        assert!(content.contains("─── unread ───"), "{content}");

        assert!(driver.hub_mut().degrade(&key("p"), 300));
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200 + FRAME_US);
        let content = tile_markdown(&scene, tile_id);
        assert!(
            content.contains("3 unread"),
            "degraded repaint keeps it: {content}"
        );
    }

    /// A portal renders even with no active tab (hud-obw3q): into the first
    /// tab, activated only once the tile exists, or into a new "Main" tab.
    #[test]
    fn drain_with_no_active_tab_activates_tab_and_paints() {
        let mut driver = InProcessPortalDriver::new();
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let main = scene.create_tab("Main", 0).unwrap();
        scene.active_tab = None; // a widget-less default tab boots inactive
        let mut processor = InputProcessor::new();
        publish(&mut driver, "p1", "NO-ACTIVE-TAB-MARKER-1", 100);
        driver.drain_inner(&mut scene, &mut processor, None, 200);
        assert_eq!(scene.active_tab, Some(main));
        assert!(tile_markdown(&scene, tile(&driver, "p1")).contains("NO-ACTIVE-TAB-MARKER-1"));

        let mut driver = InProcessPortalDriver::new();
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        publish(&mut driver, "p2", "NO-TABS-MARKER-2", 100);
        driver.drain_inner(&mut scene, &mut processor, None, 200);
        assert!(scene.active_tab.is_some());
        assert_eq!(scene.tabs.len(), 1, "exactly one default tab created");
        assert!(tile_markdown(&scene, tile(&driver, "p2")).contains("NO-TABS-MARKER-2"));
    }

    #[test]
    fn create_portal_tile_failure_does_not_mutate_active_tab() {
        let mut driver = InProcessPortalDriver::new();
        // A 1×1 display rejects the portal's bounds.
        let mut scene = SceneGraph::new(1.0, 1.0);
        scene.create_tab("Main", 0).unwrap();
        scene.active_tab = None;
        let mut processor = InputProcessor::new();
        publish(&mut driver, "p", "hello", 100);
        driver.drain_inner(&mut scene, &mut processor, None, 200);
        assert_eq!(scene.active_tab, None, "hud-zccuf");
        assert!(driver.drive.entries[&key("p")].tile_scene_id.is_none());
    }

    /// A height-only head trim (same bytes, fewer lines) still reaches
    /// `notify_head_content_removed`, so a scrolled-back offset clamps
    /// (hud-66i1s).
    #[test]
    fn head_trim_fires_on_height_shrink_without_byte_shrink_runtime_path() {
        use tze_hud_input::ScrollEvent;

        let mut driver = InProcessPortalDriver {
            hub: Arc::new(Mutex::new(PortalHub::new(PortalLimits {
                visible_bytes: 30,
                ..PortalLimits::default()
            }))),
            ..InProcessPortalDriver::new()
        };
        let (mut scene, tab, mut processor) = scene();
        publish(&mut driver, "p", "S", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200);
        let tile_id = tile(&driver, "p");
        let line_h = one_line_viewport(&mut driver, &mut scene, "p");

        publish(&mut driver, "p", &"\n".repeat(24), 300);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200 + FRAME_US);
        assert!(scene.tile_follow_tail_at_tail(tile_id));
        processor.process_scroll_event(
            &ScrollEvent {
                x: 300.0,
                y: line_h / 2.0,
                delta_x: 0.0,
                delta_y: -1.0,
            },
            &mut scene,
        );
        assert!(!scene.tile_follow_tail_at_tail(tile_id));
        let (_, pre_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(pre_y > 0.0);

        // 30 flat bytes push the 24 newlines out of the 30-byte window.
        publish(&mut driver, "p", &"B".repeat(30), 400);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200 + 2 * FRAME_US);
        processor.commit_scroll_updates(&mut scene);
        let (_, post_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(post_y <= f32::EPSILON, "pre {pre_y}, post {post_y}");
    }

    /// `hud_clear` takes the surface down on the next drain; that drain is
    /// immediate work even if a scene-lock miss deferred it.
    #[test]
    fn cleanup_work_remains_immediate_after_a_simulated_lock_miss() {
        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        publish(&mut driver, "p", "surface", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 100);
        let lease_id = lease(&driver, "p");
        assert_eq!(scene.tile_count(), 1);

        assert_eq!(driver.hub_mut().clear(&key("p")), Ok(()));
        assert_eq!(
            driver.next_wake_deadline(&scene).unwrap().family,
            PortalDeadlineFamily::ImmediateWork
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200);
        assert_eq!(scene.tile_count(), 0);
        assert!(!scene.lease_is_active(&lease_id));
        assert_eq!(
            driver.next_wake_deadline(&scene),
            None,
            "nothing left to do"
        );

        // Clear and reattach before a drain: transient absence must not reuse
        // the previous incarnation's tile or leave its lease orphaned.
        publish(&mut driver, "p", "old incarnation", 300);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 300);
        let (old_tile, old_lease) = (tile(&driver, "p"), lease(&driver, "p"));
        let old_incarnation = driver.hub_mut().incarnation(&key("p")).unwrap();
        assert_eq!(driver.hub_mut().clear(&key("p")), Ok(()));
        publish(&mut driver, "p", "latest incarnation", 350);
        assert_ne!(
            driver.hub_mut().incarnation(&key("p")),
            Some(old_incarnation)
        );
        assert_eq!(
            driver.next_wake_deadline(&scene).unwrap().family,
            PortalDeadlineFamily::ImmediateWork
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 400);
        assert_eq!(scene.tile_count(), 1);
        assert!(!scene.tiles.contains_key(&old_tile));
        assert!(scene.leases[&old_lease].state.is_terminal());
        assert_ne!(tile(&driver, "p"), old_tile);
        assert_ne!(lease(&driver, "p"), old_lease);
        let content = tile_markdown(&scene, tile(&driver, "p"));
        assert!(content.contains("latest incarnation") && !content.contains("old incarnation"));

        let live_tile = tile(&driver, "p");
        let live_lease = lease(&driver, "p");
        let handle = driver.portal_handle();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = handle.hub.lock().unwrap();
                panic!("fixture poison");
            }))
            .is_err()
        );
        let scene_version = scene.version;
        driver.drain(&mut scene, &mut processor, Some(tab));
        assert_eq!(
            scene.version, scene_version,
            "poison refuses without effects"
        );
        assert_eq!(tile(&driver, "p"), live_tile, "drive identity is preserved");
        assert_eq!(lease(&driver, "p"), live_lease);
        assert!(scene.lease_is_active(&live_lease));
        assert_eq!(
            driver.next_wake_deadline(&scene),
            None,
            "poison never busy-retries"
        );
    }

    /// Invariant 3: the viewer's dismiss reclaims the portal on the spot;
    /// `hud_hold` is then NOT_HELD and the next publish attaches fresh.
    #[test]
    fn viewer_dismiss_portal_detaches_and_next_publish_reattaches() {
        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        publish(&mut driver, "p", "live transcript", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 100);
        let old_lease = lease(&driver, "p");

        let dismissed = driver.viewer_dismiss_tile(&mut scene, tile(&driver, "p"));
        assert_eq!(
            dismissed.expiry.expect("lease reclaimed").lease_id,
            old_lease
        );
        assert_eq!(scene.tile_count(), 0, "tile removed immediately");
        assert!(!driver.drive.entries.contains_key(&key("p")));
        assert_eq!(
            hold(&mut driver, AGENT, "p", 1_000, 150),
            Err(PortalError::NotHeld)
        );

        publish(&mut driver, "p", "fresh", 200);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200);
        assert_eq!(scene.tile_count(), 1, "re-attach paints a fresh portal");
        assert_ne!(lease(&driver, "p"), old_lease);
        let content = tile_markdown(&scene, tile(&driver, "p"));
        assert!(content.contains("fresh") && !content.contains("live transcript"));
    }

    /// The scene reaped a portal's lease on its own (grace or TTL sweep):
    /// one immediate drain reclaims the portal without spinning on the past
    /// deadline, and a later publish starts fresh under a new lease.
    #[test]
    fn reattach_after_grace_expiry_starts_fresh_portal_under_new_lease() {
        let (mut driver, mut scene, tab, mut processor, clock) = clocked();
        publish(&mut driver, "p", "session-one content", clock.now_us());
        driver.drain(&mut scene, &mut processor, Some(tab));
        let (lease1, tile1) = (lease(&driver, "p"), tile(&driver, "p"));

        scene.disconnect_lease(&lease1, clock.now_millis()).unwrap();
        clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS + 1);
        assert_eq!(scene.expire_leases().len(), 1);
        assert_eq!(scene.tile_count(), 0);
        assert_eq!(
            driver.next_wake_deadline(&scene).unwrap().family,
            PortalDeadlineFamily::ImmediateWork
        );
        driver.drain(&mut scene, &mut processor, Some(tab));
        assert!(
            driver.hub_mut().get(&key("p")).is_none(),
            "the lost portal is reclaimed"
        );
        assert_eq!(driver.next_wake_deadline(&scene), None);

        publish(&mut driver, "p", "session-two content", clock.now_us());
        driver.drain(&mut scene, &mut processor, Some(tab));
        let lease2 = lease(&driver, "p");
        assert_ne!(lease2, lease1, "a fresh lease, not the dead one");
        assert!(scene.lease_is_active(&lease2));
        assert_ne!(tile(&driver, "p"), tile1);
        assert_eq!(scene.tile_count(), 1);
    }

    #[test]
    fn parked_portal_repaints_pending_count_at_input_expiry() {
        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        publish(&mut driver, "p", "waiting", 0);
        hold(&mut driver, AGENT, "p", 0, 0).unwrap();
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 0);
        let tile_id = tile(&driver, "p");
        submit(&mut driver, tile_id, "will expire", 1);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), FRAME_US);
        assert!(tile_markdown(&scene, tile_id).contains("pending HUD input: 1"));

        // Past the activity cue, only the input expiry is left to wake for.
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 3_000_000);
        let expires_us = 1 + driver.hub_mut().limits().input_ttl_us;
        assert_eq!(
            driver.next_wake_deadline(&scene),
            Some(PortalWakeDeadline {
                wall_us: expires_us,
                family: PortalDeadlineFamily::PendingInputExpiry,
            })
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab), expires_us);
        assert!(tile_markdown(&scene, tile_id).contains("pending HUD input: 0"));
        assert_ne!(
            driver.next_wake_deadline(&scene).map(|d| d.family),
            Some(PortalDeadlineFamily::PendingInputExpiry),
            "the expiry deadline is one-shot"
        );
    }

    #[test]
    fn portal_deadline_families_preserve_source_and_equal_deadline_tie() {
        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        let t = 1_000_000;
        publish(&mut driver, "p", "first", t);
        assert_eq!(
            driver.next_wake_deadline(&scene).unwrap().family,
            PortalDeadlineFamily::Cadence
        );
        driver.drain_inner(&mut scene, &mut processor, Some(tab), t);
        let cue = driver.drive.entries[&key("p")]
            .activity_cue_clear_due_us
            .expect("a fresh tail schedules its quiesce");
        assert_eq!(
            driver.next_wake_deadline(&scene),
            Some(PortalWakeDeadline {
                wall_us: cue,
                family: PortalDeadlineFamily::ActivityCue,
            })
        );

        publish(&mut driver, "p", "second", t + 100);
        let cadence = driver.next_wake_deadline(&scene).unwrap();
        assert_eq!(
            cadence,
            PortalWakeDeadline {
                wall_us: t + FRAME_US,
                family: PortalDeadlineFamily::Cadence,
            }
        );
        driver
            .drive
            .entries
            .get_mut(&key("p"))
            .unwrap()
            .activity_cue_clear_due_us = Some(cadence.wall_us);
        assert_eq!(
            driver.next_wake_deadline(&scene).unwrap().family,
            PortalDeadlineFamily::Cadence,
            "equal cadence/activity deadlines use the declared family order"
        );

        for (family, source) in [
            (
                PortalDeadlineFamily::ImmediateWork,
                RuntimeWakeupSource::SceneChange,
            ),
            (
                PortalDeadlineFamily::Cadence,
                RuntimeWakeupSource::AnimationDeadline,
            ),
            (
                PortalDeadlineFamily::ActivityCue,
                RuntimeWakeupSource::AnimationDeadline,
            ),
            (
                PortalDeadlineFamily::AgentLiveness,
                RuntimeWakeupSource::TtlDeadline,
            ),
            (
                PortalDeadlineFamily::PendingInputExpiry,
                RuntimeWakeupSource::TtlDeadline,
            ),
            (
                PortalDeadlineFamily::LeaseLifecycle,
                RuntimeWakeupSource::TtlDeadline,
            ),
        ] {
            assert_eq!(family.wakeup_source(), source, "{family:?}");
        }
    }

    /// The "⋯ writing" cue quiesces on an idle portal with one repaint, keeps
    /// the unread count, and is not repainted again (hud-kbm80).
    #[test]
    fn activity_cue_quiesces_after_deadline_without_new_update() {
        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        publish(&mut driver, "p", "streaming line", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200);
        let tile_id = tile(&driver, "p");
        let painted = tile_markdown(&scene, tile_id);
        assert!(painted.contains(PORTAL_ACTIVITY_MARKER_TEXT));
        assert!(painted.contains("1 unread"));

        let due = driver.drive.entries[&key("p")]
            .activity_cue_clear_due_us
            .expect("a fresh tail schedules a quiesce repaint");
        driver.drain_inner(&mut scene, &mut processor, Some(tab), due);
        let quiesced = tile_markdown(&scene, tile_id);
        assert!(!quiesced.contains(PORTAL_ACTIVITY_MARKER_TEXT));
        assert!(quiesced.contains("1 unread"), "no viewer action cleared it");
        assert!(
            driver.drive.entries[&key("p")]
                .activity_cue_clear_due_us
                .is_none()
        );

        let version = scene.version;
        driver.drain_inner(&mut scene, &mut processor, Some(tab), due + 1_000_000);
        assert_eq!(
            scene.version, version,
            "a quiesced idle portal is not repainted"
        );
    }

    #[test]
    fn fresh_append_extends_activity_cue_deadline() {
        let mut driver = InProcessPortalDriver::new();
        let (mut scene, tab, mut processor) = scene();
        publish(&mut driver, "p", "line one", 100);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 200);
        let tile_id = tile(&driver, "p");
        let due1 = driver.drive.entries[&key("p")]
            .activity_cue_clear_due_us
            .unwrap();

        publish(&mut driver, "p", "line two", FRAME_US);
        driver.drain_inner(&mut scene, &mut processor, Some(tab), 2 * FRAME_US);
        let due2 = driver.drive.entries[&key("p")]
            .activity_cue_clear_due_us
            .unwrap();
        assert!(due2 > due1);

        driver.drain_inner(&mut scene, &mut processor, Some(tab), due1);
        assert!(tile_markdown(&scene, tile_id).contains(PORTAL_ACTIVITY_MARKER_TEXT));
        assert_eq!(
            driver.drive.entries[&key("p")].activity_cue_clear_due_us,
            Some(due2)
        );
    }

    /// Invariant 4: an upstream drop orphans the portal (badge, content
    /// kept); the agent returning within grace resumes the SAME tile with
    /// its transcript intact and nothing duplicated.
    #[test]
    fn disconnect_then_reconnect_within_grace_resumes_same_surface_without_duplication() {
        const FIRST: &str = "ALPHA-committed-before-drop";
        const SECOND: &str = "BETA-continued-after-resume";
        let (mut driver, mut scene, tab, mut processor, clock) = clocked();
        publish(&mut driver, "p", FIRST, clock.now_us());
        driver.drain(&mut scene, &mut processor, Some(tab));
        let (tile_id, lease_id) = (tile(&driver, "p"), lease(&driver, "p"));

        clock.advance(driver.hub_mut().limits().degrade_after_us / 1_000);
        clock.advance(100);
        driver.drain(&mut scene, &mut processor, Some(tab));
        assert!(
            scene.lease_is_orphaned(&lease_id),
            "badge shown, grace running"
        );
        assert!(driver.degraded(&key("p")));

        clock.advance(5_000);
        publish(&mut driver, "p", SECOND, clock.now_us());
        driver.drain(&mut scene, &mut processor, Some(tab));
        assert_eq!(
            tile(&driver, "p"),
            tile_id,
            "resume reuses the original surface"
        );
        assert_eq!(scene.tile_count(), 1);
        assert!(scene.lease_is_active(&lease_id));
        assert!(!driver.degraded(&key("p")));
        let content = tile_markdown(&scene, tile_id);
        assert_eq!(content.matches(FIRST).count(), 1, "{content}");
        assert_eq!(content.matches(SECOND).count(), 1, "{content}");
    }

    /// Production idle sweeps degrade the portal and, 30 s later, remove it.
    /// The historical parent name is retained after transport-loss removal.
    #[test]
    fn production_ungraceful_drop_reaps_surface_on_grace_expiry_via_drain_sweep() {
        let (mut driver, mut scene, tab, mut processor, clock) = clocked();
        publish(&mut driver, "p", "committed before drop", clock.now_us());
        driver.drain(&mut scene, &mut processor, Some(tab));
        let lease_id = lease(&driver, "p");
        assert!(scene.lease_is_active(&lease_id));

        clock.advance(driver.hub_mut().limits().degrade_after_us / 1_000);
        clock.advance(100);
        let version = scene.version;
        driver.drain(&mut scene, &mut processor, Some(tab));
        assert_eq!(scene.tile_count(), 1, "degraded, not removed");
        assert!(scene.lease_is_orphaned(&lease_id));
        assert!(scene.version > version, "the dim repaints");

        clock.advance(driver.hub_mut().limits().reclaim_after_us / 1_000);
        let version = scene.version;
        driver.drain(&mut scene, &mut processor, Some(tab));
        assert_eq!(scene.tile_count(), 0, "reclaimed with no agent help");
        assert!(scene.version > version);
        assert!(!scene.lease_is_active(&lease_id));
        assert!(!driver.drive.entries.contains_key(&key("p")));
        assert!(driver.hub_mut().get(&key("p")).is_none());

        driver.drain(&mut scene, &mut processor, Some(tab));
        assert_eq!(scene.tile_count(), 0, "nothing revives");

        // Idle can arrive after accepted publishes but before the
        // first render. The hub owns these identities; renderer entries do not.
        for returning_agent in [false, true] {
            let (mut driver, mut scene, tab, mut processor, clock) = clocked();
            publish(
                &mut driver,
                "dismissed",
                "RECLAIMED-control",
                clock.now_us(),
            );
            driver.drain(&mut scene, &mut processor, Some(tab));
            let dismissed_tile = tile(&driver, "dismissed");
            driver.viewer_dismiss_tile(&mut scene, dismissed_tile);
            assert!(driver.hub_mut().get(&key("dismissed")).is_none());
            assert!(driver.drive.entries.is_empty());
            assert_eq!(scene.tile_count(), 0);

            let publications = [
                (
                    PortalKey::new("unrendered-a", "shared"),
                    "ALPHA-before-render",
                ),
                (
                    PortalKey::new("unrendered-b", "shared"),
                    "BETA-before-render",
                ),
            ];
            for (identity, text) in &publications {
                publish_for(&driver, identity, text, clock.now_us());
            }
            let alpha = &publications[0].0;
            let beta = &publications[1].0;
            assert_eq!(
                hold(&mut driver, &alpha.agent, &alpha.id, 1, clock.now_us()),
                Ok(())
            );
            driver
                .hub_mut()
                .submit_reply(alpha, "pending human input".into(), clock.now_us())
                .unwrap();
            let before: Vec<_> = publications
                .iter()
                .map(|(identity, _)| driver.hub_mut().get(identity).unwrap().clone())
                .collect();
            assert!(before.iter().all(|portal| portal.last_render_us.is_none()));
            assert!(driver.drive.entries.is_empty());
            assert_eq!(
                scene.tile_count(),
                0,
                "neither accepted publish has rendered"
            );

            // Finite hold retains content but eventually permits natural idle.
            // Indefinite hold is covered unchanged by the retained hold parent;
            // forced transport-loss degradation of it no longer exists.
            clock.advance(driver.hub_mut().limits().degrade_after_us / 1_000);
            let dropped_at_us = clock.now_us();
            let reclaim_at_us = dropped_at_us + driver.hub_mut().limits().reclaim_after_us;
            driver.drain(&mut scene, &mut processor, Some(tab));
            for ((identity, _), old) in publications.iter().zip(&before) {
                let hub = driver.hub_mut();
                let portal = hub.get(identity).unwrap();
                assert_eq!(portal.degraded_since_us, Some(dropped_at_us));
                assert_eq!(portal.status, PortalStatus::Degraded);
                assert!(!portal.dirty, "the first render consumes the idle repaint");
                assert_eq!(portal.transcript, old.transcript);
                assert_eq!(portal.replies, old.replies);
                assert_eq!(portal.inputs, old.inputs);
                assert_eq!(portal.hold_until_us, old.hold_until_us);
            }
            assert_eq!(
                driver.hub_mut().get(alpha).unwrap().hold_until_us,
                Some(before[0].last_seen_us + 1_000)
            );
            let surfaces: Vec<_> = publications
                .iter()
                .map(|(identity, _)| {
                    let entry = &driver.drive.entries[identity];
                    (entry.tile_scene_id.unwrap(), entry.scene_lease_id.unwrap())
                })
                .collect();
            assert_ne!(surfaces[0].0, surfaces[1].0);
            assert_ne!(surfaces[0].1, surfaces[1].1);
            assert_eq!(scene.tile_count(), 2);
            for ((_, marker), (tile_id, lease_id)) in publications.iter().zip(&surfaces) {
                assert!(
                    scene.lease_is_orphaned(lease_id),
                    "first drain already orphans"
                );
                let content = tile_markdown(&scene, *tile_id);
                assert_eq!(content.matches(marker).count(), 1, "{content}");
                assert!(
                    content.contains("⊘ disconnected — stream stale"),
                    "{content}"
                );
            }
            assert!(driver.hub_mut().get(&key("dismissed")).is_none());
            assert!(!scene.tiles.contains_key(&dismissed_tile));

            clock.advance(1_000);
            driver.drain(&mut scene, &mut processor, Some(tab));
            for ((identity, _), (tile_id, lease_id)) in publications.iter().zip(&surfaces) {
                assert_eq!(
                    driver.hub_mut().get(identity).unwrap().degraded_since_us,
                    Some(dropped_at_us)
                );
                assert!(
                    !driver.hub_mut().get(identity).unwrap().dirty,
                    "repeat idle drain does not dirty"
                );
                let entry = &driver.drive.entries[identity];
                assert_eq!(entry.tile_scene_id, Some(*tile_id));
                assert_eq!(entry.scene_lease_id, Some(*lease_id));
            }
            if returning_agent {
                publish_for(&driver, alpha, "ALPHA-after-resume", clock.now_us());
                driver.drain(&mut scene, &mut processor, Some(tab));
                let entry = &driver.drive.entries[alpha];
                assert_eq!(entry.tile_scene_id, Some(surfaces[0].0));
                assert_eq!(entry.scene_lease_id, Some(surfaces[0].1));
                assert!(scene.lease_is_active(&surfaces[0].1));
                assert_eq!(driver.hub_mut().get(alpha).unwrap().degraded_since_us, None);
                let content = tile_markdown(&scene, surfaces[0].0);
                assert_eq!(
                    content.matches("ALPHA-before-render").count(),
                    1,
                    "{content}"
                );
                assert_eq!(
                    content.matches("ALPHA-after-resume").count(),
                    1,
                    "{content}"
                );
                assert!(
                    !content.contains("⊘ disconnected — stream stale"),
                    "{content}"
                );
                assert!(scene.lease_is_orphaned(&surfaces[1].1));
                assert_eq!(
                    driver.hub_mut().get(beta).unwrap().degraded_since_us,
                    Some(dropped_at_us)
                );
            }

            clock.set_us(reclaim_at_us - 1);
            driver.drain(&mut scene, &mut processor, Some(tab));
            assert_eq!(
                scene.tile_count(),
                2,
                "content lasts through original grace"
            );
            for (tile_id, _) in &surfaces {
                assert!(scene.tiles.contains_key(tile_id));
            }
            assert!(scene.lease_is_orphaned(&surfaces[1].1));
            clock.set_us(reclaim_at_us);
            driver.drain(&mut scene, &mut processor, Some(tab));
            assert_eq!(scene.tile_count(), usize::from(returning_agent));
            for (index, ((identity, _), (tile_id, lease_id))) in
                publications.iter().zip(&surfaces).enumerate()
            {
                if returning_agent && index == 0 {
                    assert!(scene.tiles.contains_key(tile_id));
                    assert!(scene.lease_is_active(lease_id));
                    assert!(driver.hub_mut().get(identity).is_some());
                } else {
                    assert!(!scene.tiles.contains_key(tile_id));
                    assert!(scene.leases[lease_id].state.is_terminal());
                    assert!(!driver.drive.entries.contains_key(identity));
                    assert!(driver.hub_mut().get(identity).is_none());
                }
            }
            if !returning_agent {
                let version = scene.version;
                driver.drain(&mut scene, &mut processor, Some(tab));
                driver.drain(&mut scene, &mut processor, Some(tab));
                assert_eq!(scene.version, version, "reclaimed identities do not revive");
                assert_eq!(scene.tile_count(), 0);
                assert!(driver.drive.entries.is_empty());
                assert!(driver.hub_mut().list(AGENT).is_empty());
            }
        }
        let (mut empty, mut scene, tab, mut processor, _) = clocked();
        empty.drain(&mut scene, &mut processor, Some(tab));
        assert_eq!(scene.tile_count(), 0);
        assert!(empty.drive.entries.is_empty());
    }

    /// `hud_hold` keeps a portal past the idle reap window (degrade + reclaim)
    /// with its transcript; once the hold lapses the usual path reclaims it.
    #[test]
    fn portal_hold_outlives_idle_reap_window_then_reclaims() {
        const HOLD_MS: u64 = 300_000;
        let (mut driver, mut scene, tab, mut processor, clock) = clocked();
        publish(&mut driver, "p", "kept transcript", clock.now_us());
        driver.drain(&mut scene, &mut processor, Some(tab));
        clock.advance(3_000); // past the activity cue
        driver.drain(&mut scene, &mut processor, Some(tab));
        let lease_id = lease(&driver, "p");

        let hold_end_us = clock.now_us() + HOLD_MS * 1_000;
        hold(&mut driver, AGENT, "p", HOLD_MS, clock.now_us()).unwrap();
        assert_eq!(
            driver.next_wake_deadline(&scene),
            Some(PortalWakeDeadline {
                wall_us: hold_end_us,
                family: PortalDeadlineFamily::AgentLiveness,
            })
        );

        let limits = driver.hub_mut().limits().clone();
        let idle_reap_ms = (limits.degrade_after_us + limits.reclaim_after_us) / 1_000;
        clock.advance(2 * idle_reap_ms);
        driver.drain(&mut scene, &mut processor, Some(tab));
        assert!(scene.lease_is_active(&lease_id));
        assert!(!driver.degraded(&key("p")));
        assert!(tile_markdown(&scene, tile(&driver, "p")).contains("kept transcript"));

        clock.set_us(hold_end_us);
        driver.drain(&mut scene, &mut processor, Some(tab));
        assert!(driver.degraded(&key("p")));
        assert!(scene.lease_is_orphaned(&lease_id));
        clock.advance(limits.reclaim_after_us / 1_000);
        driver.drain(&mut scene, &mut processor, Some(tab));
        assert_eq!(scene.tile_count(), 0, "expired hold is reclaimed");
        assert!(driver.hub_mut().get(&key("p")).is_none());
    }

    /// `ttl_ms: 0` holds a portal until cleared; holds are per agent.
    #[test]
    fn portal_hold_without_ttl_never_degrades() {
        let mut driver = InProcessPortalDriver::new();
        publish(&mut driver, "p", "x", 1_000);
        assert_eq!(hold(&mut driver, AGENT, "p", 0, 1_000), Ok(()));
        driver.hub_mut().take_due(1_000);
        assert_eq!(driver.hub_mut().next_deadline(), None);
        assert!(driver.hub_mut().sweep(u64::MAX - 1).is_empty());
        assert_eq!(
            hold(&mut driver, "someone-else", "p", 0, 2_000),
            Err(PortalError::NotHeld),
            "another agent's portal:p is not this one"
        );
    }
}
