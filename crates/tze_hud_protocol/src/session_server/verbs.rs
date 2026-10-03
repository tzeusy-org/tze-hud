//! Lifecycle verbs on the resident session stream (`docs/api.md`):
//! `ClaimTile`, `Publish`, `Clear`, and `Hold`, plus the one reply shape,
//! `RequestResult`.
//!
//! Surfaces are the strings MCP uses (`zone:<name>`, `widget:<name>`) plus
//! `tile:<uuid>` for tiles from `ClaimTile`. Failures carry a code from the
//! shared closed set (`tze_hud_scene::error_codes::ERROR_CODES`) and a hint.
//!
//! `ClaimTile`, `Hold`, and `Clear` replies are cached by client sequence so a
//! retransmit replays the reply instead of granting a second lease.

use std::sync::Arc;

use tokio::sync::Mutex;
use tonic::Status;
use tze_hud_scene::element_store::ElementType;
use tze_hud_scene::error_codes::{ERROR_CODES, validation_error_code};
use tze_hud_scene::mutation::{MutationBatch as SceneMutationBatch, SceneMutation};
use tze_hud_scene::placement::{TileAnchor, TileSize, resolve_tile_placement};
use tze_hud_scene::types::SceneId;

use crate::proto::session::server_message::Payload as ServerPayload;
use crate::proto::session::*;
use crate::session::SharedState;

use super::MutationBudgetDecision;
use super::stream_session::StreamSession;
use super::{
    capability_set_covers, now_ms, now_wall_us, persist_created_tile_entries,
    persist_element_store, scene_id_to_bytes, touch_element_store_entry_by_namespace,
};

/// TTL for a tile lease when `ClaimTile` / `Hold` send 0.
const DEFAULT_TILE_TTL_MS: u64 = 60_000;

// ─── RequestResult ───────────────────────────────────────────────────────────

pub(super) fn ok(seq: u64) -> RequestResult {
    RequestResult {
        seq,
        ok: true,
        ..Default::default()
    }
}

/// A failed request. `code` must be in the shared closed set.
pub(super) fn fail(seq: u64, code: &str, hint: impl Into<String>) -> RequestResult {
    debug_assert!(ERROR_CODES.contains(&code), "{code} not in ERROR_CODES");
    RequestResult {
        seq,
        ok: false,
        code: code.to_string(),
        hint: hint.into(),
        ..Default::default()
    }
}

/// The `RequestResult` payload for a `MutationBatch`.
pub(super) fn batch_result(
    seq: u64,
    batch_id: Vec<u8>,
    accepted: bool,
    created_ids: Vec<Vec<u8>>,
    code: String,
    hint: String,
) -> ServerPayload {
    debug_assert!(code.is_empty() || ERROR_CODES.contains(&code.as_str()));
    ServerPayload::RequestResult(RequestResult {
        seq,
        ok: accepted,
        code,
        hint,
        ids: created_ids,
        batch_id,
        ..Default::default()
    })
}

pub(super) async fn send_result(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    result: RequestResult,
) {
    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::RequestResult(result)),
        }))
        .await;
}

/// Send `result` and cache it under `seq` for retransmit replay.
async fn send_cached(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    result: RequestResult,
) {
    if result.seq > 0 {
        session
            .lease_correlation_cache
            .insert(result.seq, result.clone());
    }
    send_result(session, tx, result).await;
}

/// Replay a cached reply for a retransmitted request. Returns whether it did.
async fn replay_cached(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    seq: u64,
) -> bool {
    if seq == 0 {
        return false;
    }
    let Some(cached) = session.lease_correlation_cache.get(seq).cloned() else {
        return false;
    };
    send_result(session, tx, cached).await;
    true
}

// ─── Surfaces ────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Surface {
    Zone(String),
    Widget(String),
    Tile(SceneId),
}

impl Surface {
    pub(super) fn parse(s: &str) -> Result<Self, &'static str> {
        const BAD: &str = "surface is zone:<name>, widget:<name>, or tile:<id>";
        let (kind, name) = s.split_once(':').ok_or(BAD)?;
        if name.is_empty() {
            return Err(BAD);
        }
        match kind {
            "zone" => Ok(Self::Zone(name.to_string())),
            "widget" => Ok(Self::Widget(name.to_string())),
            "tile" => uuid::Uuid::parse_str(name)
                .map(|u| Self::Tile(SceneId::from_uuid(u)))
                .map_err(|_| "tile:<id> takes the uuid from ClaimTile"),
            _ => Err(BAD),
        }
    }

    /// The internal permission this surface needs, and the allow entry that grants it.
    fn permission(&self) -> (String, String) {
        match self {
            Self::Zone(n) => (format!("publish_zone:{n}"), format!("zone:{n}")),
            Self::Widget(n) => (format!("publish_widget:{n}"), format!("widget:{n}")),
            Self::Tile(_) => ("create_tiles".to_string(), "tiles".to_string()),
        }
    }
}

/// The surface string for a tile.
pub fn tile_surface(tile_id: SceneId) -> String {
    format!("tile:{}", tile_id.as_uuid())
}

/// Parse `surface` and check the agent's allow list covers it.
fn allowed_surface(
    session: &StreamSession,
    seq: u64,
    s: &str,
) -> Result<Surface, Box<RequestResult>> {
    let surface =
        Surface::parse(s).map_err(|hint| Box::new(fail(seq, "INVALID_ARGUMENT", hint)))?;
    let (permission, entry) = surface.permission();
    if capability_set_covers(&session.capabilities, &permission) {
        Ok(surface)
    } else {
        Err(Box::new(not_allowed(seq, &session.agent_name, &entry)))
    }
}

fn not_allowed(seq: u64, agent: &str, entry: &str) -> RequestResult {
    fail(
        seq,
        "NOT_ALLOWED",
        format!("needs \"{entry}\" in [agents.{agent}] allow"),
    )
}

async fn safe_mode_active(state: &Arc<Mutex<SharedState>>, session: &StreamSession) -> bool {
    session.safe_mode_active
        || state
            .lock()
            .await
            .safe_mode_atomic
            .load(std::sync::atomic::Ordering::Acquire)
}

fn safe_mode(seq: u64) -> RequestResult {
    fail(
        seq,
        "SAFE_MODE_ACTIVE",
        "the human paused agents; retry after SessionResumed",
    )
}

/// The lease behind `tile`, if this session's namespace owns it.
fn owned_tile_lease(
    scene: &tze_hud_scene::graph::SceneGraph,
    namespace: &str,
    tile: SceneId,
) -> Option<SceneId> {
    scene
        .tiles
        .get(&tile)
        .filter(|t| t.namespace == namespace)
        .map(|t| t.lease_id)
}

fn not_held(seq: u64, surface: &str) -> RequestResult {
    fail(
        seq,
        "NOT_HELD",
        format!("you hold nothing on {surface}; ClaimTile or Publish first"),
    )
}

// ─── ClaimTile ───────────────────────────────────────────────────────────────

fn anchor_from_proto(v: i32) -> TileAnchor {
    match TileAnchor_::try_from(v).unwrap_or(TileAnchor_::Unspecified) {
        TileAnchor_::TopLeft => TileAnchor::TopLeft,
        TileAnchor_::Top => TileAnchor::Top,
        TileAnchor_::TopRight | TileAnchor_::Unspecified => TileAnchor::TopRight,
        TileAnchor_::Left => TileAnchor::Left,
        TileAnchor_::Center => TileAnchor::Center,
        TileAnchor_::Right => TileAnchor::Right,
        TileAnchor_::BottomLeft => TileAnchor::BottomLeft,
        TileAnchor_::Bottom => TileAnchor::Bottom,
        TileAnchor_::BottomRight => TileAnchor::BottomRight,
    }
}

fn size_from_proto(v: i32) -> TileSize {
    match TileSize_::try_from(v).unwrap_or(TileSize_::Unspecified) {
        TileSize_::Small => TileSize::Small,
        TileSize_::Medium | TileSize_::Unspecified => TileSize::Medium,
        TileSize_::Large => TileSize::Large,
        TileSize_::Wide => TileSize::Wide,
        TileSize_::Tall => TileSize::Tall,
    }
}

use crate::proto::session::TileAnchor as TileAnchor_;
use crate::proto::session::TileSize as TileSize_;

/// Grant a lease, create the tile at the resolved placement, and apply the
/// initial node tree, all under one scene lock. Any failure after the grant
/// revokes the lease, which removes the tile.
pub(super) async fn handle_claim_tile(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    seq: u64,
    claim: ClaimTile,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) {
    if replay_cached(session, tx, seq).await {
        return;
    }
    if !capability_set_covers(&session.capabilities, "create_tiles") {
        let r = not_allowed(seq, &session.agent_name, "tiles");
        return send_cached(session, tx, r).await;
    }
    if safe_mode_active(state, session).await {
        return send_result(session, tx, safe_mode(seq)).await;
    }
    let root_nodes = match claim.root.as_ref() {
        None => Vec::new(),
        Some(root) => match crate::convert::proto_node_tree_to_scene(root) {
            Some(nodes) => nodes,
            None => {
                let r = fail(seq, "INVALID_ARGUMENT", "root node has no data");
                return send_cached(session, tx, r).await;
            }
        },
    };
    let ttl = if claim.ttl_ms > 0 {
        claim.ttl_ms
    } else {
        DEFAULT_TILE_TTL_MS
    };
    let placement = claim.placement.unwrap_or_default();

    let mut st = state.lock().await;
    let tokens = st.tile_placement.clone();
    let outcome = {
        let mut scene = st.scene.lock().await;
        claim_in_scene(
            &mut scene,
            session,
            ttl,
            &tokens,
            anchor_from_proto(placement.anchor),
            size_from_proto(placement.size),
            root_nodes,
        )
    };
    let (tile_id, lease_id, node_ids) = match outcome {
        Ok(v) => v,
        Err((code, hint)) => {
            drop(st);
            return send_cached(session, tx, fail(seq, code, hint)).await;
        }
    };
    {
        let scene = st.scene.lock().await;
        st.refresh_active_tab_mirror(&scene);
    }
    session.lease_ids.push(lease_id);
    let persist = persist_created_tile_entries(&mut st, &[tile_id]).await;
    drop(st);
    render_wake.notify();
    persist_element_store(persist).await;

    let mut ids = vec![scene_id_to_bytes(tile_id)];
    ids.extend(node_ids.into_iter().map(scene_id_to_bytes));
    let result = RequestResult {
        ids,
        lease_id: scene_id_to_bytes(lease_id),
        ttl_ms: ttl,
        ..ok(seq)
    };
    send_cached(session, tx, result).await;
}

type ClaimOutcome = Result<(SceneId, SceneId, Vec<SceneId>), (&'static str, String)>;

fn claim_in_scene(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    session: &StreamSession,
    ttl: u64,
    tokens: &tze_hud_scene::placement::TilePlacementTokens,
    anchor: TileAnchor,
    size: TileSize,
    root_nodes: Vec<tze_hud_scene::types::Node>,
) -> ClaimOutcome {
    let lease_id = scene
        .try_grant_lease_for_session_with_budget(
            &session.namespace,
            session.scene_session_id,
            ttl,
            session.resource_budget.clone(),
        )
        .map_err(|e| ("BUDGET_EXCEEDED", e.to_string()))?;
    let result = place_and_fill(scene, session, lease_id, tokens, anchor, size, root_nodes);
    if result.is_err() {
        let _ = scene.revoke_lease(lease_id);
    }
    result.map(|(tile, nodes)| (tile, lease_id, nodes))
}

fn place_and_fill(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    session: &StreamSession,
    lease_id: SceneId,
    tokens: &tze_hud_scene::placement::TilePlacementTokens,
    anchor: TileAnchor,
    size: TileSize,
    mut root_nodes: Vec<tze_hud_scene::types::Node>,
) -> Result<(SceneId, Vec<SceneId>), (&'static str, String)> {
    // A scene with no tab yet (a fresh headless runtime) gets one; the
    // runtime owns tabs, so this is its call, not the agent's.
    let tab_id = match scene.active_tab {
        Some(tab) => tab,
        None => {
            let tab = scene
                .create_tab("main", 0)
                .map_err(|e| ("UNAVAILABLE", e.to_string()))?;
            scene.active_tab = Some(tab);
            tab
        }
    };
    // Agent tiles on this tab: placement steps past them, and the new tile
    // goes above them (ties go to claim order).
    let agent_tiles: Vec<_> = scene
        .tiles
        .values()
        .filter(|t| t.tab_id == tab_id && t.z_order < tze_hud_scene::ZONE_TILE_Z_MIN)
        .map(|t| (t.bounds, t.z_order))
        .collect();
    let occupied: Vec<_> = agent_tiles.iter().map(|(b, _)| *b).collect();
    let bounds = resolve_tile_placement(tokens, anchor, size, scene.display_area, &occupied);
    let z_order = agent_tiles
        .iter()
        .map(|(_, z)| z + 1)
        .max()
        .unwrap_or(1)
        .min(tze_hud_scene::ZONE_TILE_Z_MIN - 1);

    let mut mutations = vec![SceneMutation::CreateTile {
        tab_id,
        namespace: session.namespace.clone(),
        lease_id,
        bounds,
        z_order,
    }];
    let create = apply(scene, session, lease_id, std::mem::take(&mut mutations))?;
    let tile_id = *create
        .first()
        .ok_or(("INTERNAL", "tile was not created".to_string()))?;
    if root_nodes.is_empty() {
        return Ok((tile_id, Vec::new()));
    }
    let node_ids = root_nodes.iter().map(|n| n.id).collect();
    let node = root_nodes.remove(0);
    apply(
        scene,
        session,
        lease_id,
        vec![SceneMutation::SetTileRoot {
            tile_id,
            node,
            descendants: root_nodes,
        }],
    )?;
    Ok((tile_id, node_ids))
}

/// Apply `mutations` as one batch under the session's budget; the scene's
/// reason becomes the hint on rejection.
fn apply(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    session: &StreamSession,
    lease_id: SceneId,
    mutations: Vec<SceneMutation>,
) -> Result<Vec<SceneId>, (&'static str, String)> {
    let batch = SceneMutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: session.namespace.clone(),
        mutations,
        timing_hints: None,
        lease_id: Some(lease_id),
    };
    let delta = scene.mutation_budget_delta(&lease_id, &batch);
    if let Some(enforcer) = &session.budget_enforcer
        && let MutationBudgetDecision::Reject {
            error_code,
            message,
        } = enforcer.reserve_mutation(
            session.scene_session_id,
            delta.delta_tiles,
            delta.delta_texture_bytes,
            delta.max_nodes_in_batch,
        )
    {
        return Err(("BUDGET_EXCEEDED", format!("{error_code}: {message}")));
    }
    let result = scene.apply_batch(&batch);
    if result.applied {
        return Ok(result.created_ids);
    }
    if let Some(enforcer) = &session.budget_enforcer {
        enforcer.rollback_mutation(
            session.scene_session_id,
            delta.delta_tiles,
            delta.delta_texture_bytes,
        );
    }
    Err(match result.error {
        Some(e) => (validation_error_code(&e), e.to_string()),
        None => ("INTERNAL", "batch was not applied".to_string()),
    })
}

// ─── Publish ─────────────────────────────────────────────────────────────────

/// Publish to a zone or widget. Durable surfaces reply with `RequestResult`;
/// ephemeral ones are fire-and-forget, success or failure (invariant 2).
pub(super) async fn handle_publish(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    seq: u64,
    publish: Publish,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) {
    let surface = match allowed_surface(session, seq, &publish.surface) {
        Ok(s) => s,
        Err(r) => return send_result(session, tx, *r).await,
    };
    let (result, ephemeral, persist) = {
        let mut st = state.lock().await;
        let mut scene = st.scene.lock().await;
        let (result, ephemeral, touched) = match &surface {
            Surface::Zone(zone) => {
                let ephemeral = scene
                    .zone_registry
                    .get_by_name(zone)
                    .is_some_and(|d| d.ephemeral);
                let r = publish_zone(&mut scene, session, seq, zone, &publish);
                (r, ephemeral, Some((ElementType::Zone, zone.clone())))
            }
            Surface::Widget(widget) => {
                let ephemeral = scene
                    .widget_registry
                    .instances
                    .get(widget)
                    .and_then(|i| scene.widget_registry.definitions.get(&i.widget_type_name))
                    .is_some_and(|d| d.ephemeral);
                let r = publish_widget(&mut scene, session, seq, widget, &publish);
                (r, ephemeral, Some((ElementType::Widget, widget.clone())))
            }
            Surface::Tile(_) => (
                fail(
                    seq,
                    "INVALID_ARGUMENT",
                    "tiles take content via ClaimTile root or MutationBatch",
                ),
                false,
                None,
            ),
        };
        drop(scene);
        let persist = match touched {
            Some((ty, name)) if result.ok => {
                touch_element_store_entry_by_namespace(&mut st, ty, &name, now_ms())
            }
            _ => None,
        };
        (result, ephemeral, persist)
    };
    if result.ok {
        render_wake.notify();
    }
    persist_element_store(persist).await;
    if !ephemeral {
        send_result(session, tx, result).await;
    }
}

fn publish_zone(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    session: &StreamSession,
    seq: u64,
    zone: &str,
    publish: &Publish,
) -> RequestResult {
    let Some(content) = publish
        .content
        .as_ref()
        .and_then(crate::convert::proto_zone_content_to_scene)
    else {
        return fail(seq, "INVALID_ARGUMENT", "zone publish needs content");
    };
    if !publish.breakpoints.is_empty()
        && !matches!(content, tze_hud_scene::types::ZoneContent::StreamText(_))
    {
        return fail(
            seq,
            "INVALID_ARGUMENT",
            "breakpoints are only valid for stream_text content",
        );
    }
    let now = scene.now_wall_us();
    let zone_exists = scene.zone_registry.get_by_name(zone).is_some();
    if let Some((code, hint)) = timing_error(publish, now, zone, zone_exists) {
        return fail(seq, code, hint);
    }
    let batch = SceneMutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: session.namespace.clone(),
        mutations: vec![SceneMutation::PublishToZone {
            zone_name: zone.to_string(),
            content,
            publish_token: tze_hud_scene::types::ZonePublishToken { token: Vec::new() },
            merge_key: (!publish.key.is_empty()).then(|| publish.key.clone()),
            // Content expiry (invariant 1): expires_at wins, else ttl_ms
            // counts from presentation.
            expires_at_wall_us: expires_at_wall_us(publish, now),
            content_classification: None,
            breakpoints: publish.breakpoints.clone(),
            held: false,
        }],
        timing_hints: None,
        lease_id: session.lease_ids.first().copied(),
    };
    // present_at in the future: hold the publish until due (invariant 1).
    if publish.present_at_us > now {
        scene.schedule_batch(publish.present_at_us, batch);
        return ok(seq);
    }
    let result = scene.apply_batch(&batch);
    match result.error {
        None if result.applied => ok(seq),
        Some(e) => fail(seq, validation_error_code(&e), e.to_string()),
        None => fail(seq, "INTERNAL", "zone publish was not applied"),
    }
}

fn publish_widget(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    session: &StreamSession,
    seq: u64,
    widget: &str,
    publish: &Publish,
) -> RequestResult {
    let params = publish
        .params
        .iter()
        .filter_map(crate::convert::proto_to_widget_param_value)
        .collect();
    let now = scene.now_wall_us();
    match scene.publish_to_widget_for_lease(
        widget,
        params,
        &session.namespace,
        (!publish.key.is_empty()).then(|| publish.key.clone()),
        publish.transition_ms,
        expires_at_wall_us(publish, now),
        session.lease_ids.first().copied(),
    ) {
        Ok(_) => ok(seq),
        Err(e) => fail(seq, validation_error_code(&e), e.to_string()),
    }
}

/// Absolute content expiry: `expires_at_us` if set, else `ttl_ms` counted
/// from presentation (now, or `present_at_us` if later), else none.
fn expires_at_wall_us(publish: &Publish, now_wall_us: u64) -> Option<u64> {
    if publish.expires_at_us > 0 {
        Some(publish.expires_at_us)
    } else if publish.ttl_ms > 0 {
        let shown_at = publish.present_at_us.max(now_wall_us);
        Some(shown_at.saturating_add(publish.ttl_ms.saturating_mul(1_000)))
    } else {
        None
    }
}

/// Reject timing that can never display, and scheduled publishes to unknown
/// zones (they would otherwise fail silently when due).
fn timing_error(
    publish: &Publish,
    now_wall_us: u64,
    zone: &str,
    zone_exists: bool,
) -> Option<(&'static str, String)> {
    let present = publish.present_at_us;
    if present > now_wall_us.saturating_add(super::DEFAULT_MAX_FUTURE_SCHEDULE_US) {
        return Some((
            "TIMESTAMP_TOO_FUTURE",
            format!("present_at_us ({present}) is beyond the scheduling horizon"),
        ));
    }
    if let Some(expires) = expires_at_wall_us(publish, now_wall_us)
        && expires <= present.max(now_wall_us)
    {
        return Some((
            "TIMESTAMP_EXPIRY_BEFORE_PRESENT",
            format!("expiry ({expires}) must be after the presentation time"),
        ));
    }
    if present > now_wall_us && !zone_exists {
        return Some(("ZONE_NOT_FOUND", format!("no zone {zone}")));
    }
    None
}

// ─── Clear ───────────────────────────────────────────────────────────────────

/// Release a zone publication, a widget publication, or a tile (its lease).
pub(super) async fn handle_clear(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    seq: u64,
    clear: Clear,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) {
    if replay_cached(session, tx, seq).await {
        return;
    }
    let surface = match allowed_surface(session, seq, &clear.surface) {
        Ok(s) => s,
        Err(r) => return send_result(session, tx, *r).await,
    };
    let mut released = None;
    let result = {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        let r = match &surface {
            Surface::Zone(zone) => scene.clear_zone_for_publisher(zone, &session.namespace),
            Surface::Widget(widget) => scene.clear_widget_for_publisher(widget, &session.namespace),
            Surface::Tile(tile) => match owned_tile_lease(&scene, &session.namespace, *tile) {
                Some(lease) => {
                    released = Some(lease);
                    scene.revoke_lease(lease)
                }
                None => {
                    drop(scene);
                    drop(st);
                    return send_result(session, tx, not_held(seq, &clear.surface)).await;
                }
            },
        };
        match r {
            Ok(()) => ok(seq),
            Err(e) => fail(seq, validation_error_code(&e), e.to_string()),
        }
    };
    if result.ok {
        if let Some(lease) = released {
            session.lease_ids.retain(|id| *id != lease);
        }
        render_wake.notify();
    }
    send_cached(session, tx, result).await;
}

// ─── Hold ────────────────────────────────────────────────────────────────────

/// Extend a holding without resending content.
pub(super) async fn handle_hold(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    seq: u64,
    hold: Hold,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) {
    if replay_cached(session, tx, seq).await {
        return;
    }
    let surface = match allowed_surface(session, seq, &hold.surface) {
        Ok(s) => s,
        Err(r) => return send_result(session, tx, *r).await,
    };
    let result = {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        let expires =
            (hold.ttl_ms > 0).then(|| scene.now_wall_us().saturating_add(hold.ttl_ms * 1_000));
        let held_ttl = RequestResult {
            ttl_ms: hold.ttl_ms,
            ..ok(seq)
        };
        match &surface {
            Surface::Zone(zone) => {
                if scene.hold_zone_publications(zone, &session.namespace, expires) {
                    held_ttl
                } else {
                    not_held(seq, &hold.surface)
                }
            }
            Surface::Widget(widget) => {
                if scene.hold_widget_publications(widget, &session.namespace, expires) {
                    held_ttl
                } else {
                    not_held(seq, &hold.surface)
                }
            }
            Surface::Tile(tile) => match owned_tile_lease(&scene, &session.namespace, *tile) {
                Some(lease) => {
                    let ttl = if hold.ttl_ms > 0 {
                        hold.ttl_ms
                    } else {
                        DEFAULT_TILE_TTL_MS
                    };
                    match scene.renew_lease(lease, ttl) {
                        Ok(()) => RequestResult {
                            lease_id: scene_id_to_bytes(lease),
                            ttl_ms: ttl,
                            ..ok(seq)
                        },
                        Err(e) => fail(seq, validation_error_code(&e), e.to_string()),
                    }
                }
                None => not_held(seq, &hold.surface),
            },
        }
    };
    if result.ok {
        render_wake.notify();
    }
    send_cached(session, tx, result).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_parse_like_mcp() {
        assert_eq!(
            Surface::parse("zone:subtitle"),
            Ok(Surface::Zone("subtitle".into()))
        );
        assert_eq!(
            Surface::parse("widget:gauge"),
            Ok(Surface::Widget("gauge".into()))
        );
        let id = SceneId::new();
        assert_eq!(Surface::parse(&tile_surface(id)), Ok(Surface::Tile(id)));
        assert!(Surface::parse("tile:nope").is_err());
        assert!(Surface::parse("zone:").is_err());
        assert!(Surface::parse("portal:x").is_err());
    }

    /// Every code the session server can put in a `RequestResult` is in the
    /// shared closed set (invariant 8).
    #[test]
    fn grpc_codes_are_in_the_shared_set() {
        let sources = [include_str!("verbs.rs"), include_str!("mutations.rs")];
        let re = regex_lite_codes();
        for src in sources {
            for code in re(src) {
                assert!(
                    ERROR_CODES.contains(&code.as_str()),
                    "{code} not in ERROR_CODES"
                );
            }
        }
    }

    /// Quoted SHOUTY_SNAKE strings passed as codes: `fail(seq, "X"`, a
    /// `("X", …)` error tuple, or `"X".to_string()` in a code position.
    fn regex_lite_codes() -> impl Fn(&str) -> Vec<String> {
        |src: &str| {
            let mut out = Vec::new();
            for line in src.lines() {
                let t = line.trim_start();
                if t.starts_with("//") || t.starts_with("assert") {
                    continue;
                }
                let mut rest = line;
                while let Some(i) = rest.find('"') {
                    let after = &rest[i + 1..];
                    let Some(j) = after.find('"') else { break };
                    let lit = &after[..j];
                    let is_code = lit.len() > 3
                        && lit.contains('_')
                        && lit.chars().all(|c| c.is_ascii_uppercase() || c == '_');
                    let next = &after[j + 1..];
                    if is_code
                        && (next.starts_with(".to_string()")
                            || next.starts_with(',')
                            || next.starts_with(')'))
                        && !KNOWN_NON_CODES.contains(&lit)
                    {
                        out.push(lit.to_string());
                    }
                    rest = next;
                }
            }
            out
        }
    }

    /// Subscription category names, which share the SHOUTY_SNAKE style.
    const KNOWN_NON_CODES: &[&str] = &[
        "SCENE_TOPOLOGY",
        "INPUT_EVENTS",
        "FOCUS_EVENTS",
        "DEGRADATION_NOTICES",
        "TELEMETRY_FRAMES",
    ];
}
