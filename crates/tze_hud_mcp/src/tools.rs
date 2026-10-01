//! MCP tool implementations.
//!
//! Each function takes `params: serde_json::Value` and a mutable reference to
//! the shared scene state, and returns a [`McpResult`] with a serializable
//! response value.
//!
//! Tool naming follows the issue spec:
//! - `create_tab`        → `handle_create_tab`
//! - `create_tile`       → `handle_create_tile`
//! - `set_content`       → `handle_set_content`
//! - `dismiss`           → `handle_dismiss`
//! - `publish_to_zone`   → `handle_publish_to_zone`
//! - `list_zones`        → `handle_list_zones`
//! - `list_scene`        → `handle_list_scene`
//! - `register_widget_asset` → `handle_register_widget_asset`
//! - `publish_to_widget` → `handle_publish_to_widget`
//! - `list_widgets`      → `handle_list_widgets`
//! - `clear_widget`      → `handle_clear_widget`
//! - `list_elements`     → `handle_list_elements`
//! - `publish_to_element`→ `handle_publish_to_element`

use crate::{error::McpError, types::McpResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tze_hud_scene::{
    graph::SceneGraph,
    types::{
        Capability, FontFamily, GeometryPolicy, Node, NodeData, NotificationAction,
        NotificationPayload, Rect, Rgba, SceneId, StatusBarPayload, TextAlign, TextMarkdownNode,
        TextOverflow, WidgetParameterValue, ZoneContent, geometry_policy_to_absolute_rect,
        rect_to_relative_geometry_policy,
    },
};

// ─── create_tab ─────────────────────────────────────────────────────────────

/// Parameters for `create_tab`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateTabParams {
    /// Human-readable name for the tab.
    pub name: String,
    /// Display order (must be unique across tabs). Defaults to next available.
    #[serde(default)]
    pub display_order: Option<u32>,
}

/// Response from `create_tab`.
#[derive(Debug, Serialize)]
pub struct CreateTabResult {
    /// The UUID of the newly created tab.
    pub tab_id: String,
    /// The name given to the tab.
    pub name: String,
    /// The display order assigned to this tab.
    pub display_order: u32,
}

/// Create a new tab in the scene.
///
/// If `display_order` is omitted, the next available order (max + 1) is used.
///
/// # Errors
/// - `invalid_params` if `name` is empty.
/// - `scene_error` if `display_order` is already taken.
pub fn handle_create_tab(params: Value, scene: &mut SceneGraph) -> McpResult<CreateTabResult> {
    let p: CreateTabParams = parse_params(params)?;

    if p.name.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "name must be non-empty".to_string(),
        ));
    }

    let order = p.display_order.unwrap_or_else(|| {
        scene
            .tabs
            .values()
            .map(|t| t.display_order)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0)
    });

    let tab_id = scene.create_tab(&p.name, order)?;

    Ok(CreateTabResult {
        tab_id: tab_id.to_string(),
        name: p.name,
        display_order: order,
    })
}

// ─── create_tile ────────────────────────────────────────────────────────────

/// Parameters for `create_tile`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateTileParams {
    /// ID of the tab to place the tile in. If omitted, uses the active tab.
    pub tab_id: Option<String>,
    /// Namespace (agent identity) for the tile. Used as the lease namespace.
    pub namespace: String,
    /// Bounds: x, y, width, height in display pixels.
    pub bounds: BoundsParams,
    /// Z-order (front = higher). Defaults to 1.
    #[serde(default = "default_z_order")]
    pub z_order: u32,
    /// Lease TTL in milliseconds. Defaults to 60 000 (1 minute).
    #[serde(default = "default_ttl_ms")]
    pub ttl_ms: u64,
}

/// Bounds as a plain JSON sub-object.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoundsParams {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

fn default_z_order() -> u32 {
    1
}

fn default_ttl_ms() -> u64 {
    60_000
}

/// Response from `create_tile`.
#[derive(Debug, Serialize)]
pub struct CreateTileResult {
    /// UUID of the newly created tile.
    pub tile_id: String,
    /// UUID of the lease granted to this tile.
    pub lease_id: String,
    /// The tab this tile belongs to.
    pub tab_id: String,
    /// Namespace under which the lease was granted.
    pub namespace: String,
}

/// Create a tile within a tab.
///
/// Automatically grants a lease for the tile with `CreateTile`, `UpdateTile`,
/// `CreateNode`, and `UpdateNode` capabilities.
///
/// # Errors
/// - `invalid_params` if namespace is empty or bounds are invalid.
/// - `no_active_tab` if `tab_id` is omitted and no tab is active.
/// - `invalid_id` if `tab_id` is provided but not a valid UUID.
/// - `scene_error` if the tab does not exist.
pub fn handle_create_tile(params: Value, scene: &mut SceneGraph) -> McpResult<CreateTileResult> {
    let p: CreateTileParams = parse_params(params)?;

    if p.namespace.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "namespace must be non-empty".to_string(),
        ));
    }

    if p.bounds.width <= 0.0 || p.bounds.height <= 0.0 {
        return Err(McpError::InvalidParams(
            "bounds.width and bounds.height must be > 0".to_string(),
        ));
    }

    // Resolve tab ID
    let tab_id = match p.tab_id {
        Some(ref s) => parse_scene_id(s)?,
        None => scene.active_tab.ok_or(McpError::NoActiveTab)?,
    };

    // Grant a lease with sufficient capabilities for tile+content operations
    let lease_id = scene.grant_lease(
        &p.namespace,
        p.ttl_ms,
        vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
    );

    let bounds = Rect::new(p.bounds.x, p.bounds.y, p.bounds.width, p.bounds.height);
    let tile_id = scene.create_tile(tab_id, &p.namespace, lease_id, bounds, p.z_order)?;

    Ok(CreateTileResult {
        tile_id: tile_id.to_string(),
        lease_id: lease_id.to_string(),
        tab_id: tab_id.to_string(),
        namespace: p.namespace,
    })
}

// ─── set_content ─────────────────────────────────────────────────────────────

/// Parameters for `set_content`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetContentParams {
    /// ID of the tile to set content on.
    pub tile_id: String,
    /// Markdown text to display.
    pub content: String,
    /// Font size in pixels. Defaults to 16.
    #[serde(default = "default_font_size")]
    pub font_size_px: f32,
    /// Hex or well-known color string for text. Defaults to white (#ffffff).
    #[serde(default = "default_color")]
    pub color: ColorParams,
    /// Hex or well-known color string for background. Optional.
    pub background: Option<ColorParams>,
    /// Text alignment: "start", "center", or "end". Defaults to "start".
    #[serde(default = "default_alignment")]
    pub alignment: String,
}

/// RGBA color as individual channels in [0.0, 1.0].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ColorParams {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    #[serde(default = "default_alpha")]
    pub a: f32,
}

fn default_font_size() -> f32 {
    16.0
}

fn default_color() -> ColorParams {
    ColorParams {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    }
}

fn default_alpha() -> f32 {
    1.0
}

fn default_alignment() -> String {
    "start".to_string()
}

/// Response from `set_content`.
#[derive(Debug, Serialize)]
pub struct SetContentResult {
    /// UUID of the tile that was updated.
    pub tile_id: String,
    /// UUID of the new root node created to hold the content.
    pub node_id: String,
    /// Number of characters in the content.
    pub content_len: usize,
}

/// Set markdown text content on a tile.
///
/// Replaces the tile's root node with a [`TextMarkdownNode`] spanning the
/// full tile bounds. Any previous root node is discarded.
///
/// # Errors
/// - `invalid_params` if `tile_id` is not a valid UUID or content is empty.
/// - `invalid_id` if `tile_id` is malformed.
/// - `scene_error` if the tile does not exist.
pub fn handle_set_content(params: Value, scene: &mut SceneGraph) -> McpResult<SetContentResult> {
    let p: SetContentParams = parse_params(params)?;

    if p.content.is_empty() {
        return Err(McpError::InvalidParams(
            "content must be non-empty".to_string(),
        ));
    }

    if p.font_size_px <= 0.0 {
        return Err(McpError::InvalidParams(
            "font_size_px must be > 0".to_string(),
        ));
    }

    let tile_id = parse_scene_id(&p.tile_id)?;

    // Look up the tile to get its bounds for the text node
    let tile_bounds = scene
        .tiles
        .get(&tile_id)
        .ok_or_else(|| McpError::SceneError(format!("tile not found: {tile_id}")))?
        .bounds;

    let alignment = match p.alignment.as_str() {
        "center" => TextAlign::Center,
        "end" => TextAlign::End,
        _ => TextAlign::Start,
    };

    let color = Rgba::new(p.color.r, p.color.g, p.color.b, p.color.a);
    let background = p.background.map(|bg| Rgba::new(bg.r, bg.g, bg.b, bg.a));

    let node_id = SceneId::new();
    let node = Node {
        layout: Default::default(),
        id: node_id,
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: p.content.clone(),
            // Fill the entire tile
            bounds: Rect::new(0.0, 0.0, tile_bounds.width, tile_bounds.height),
            font_size_px: p.font_size_px,
            font_family: FontFamily::SystemSansSerif,
            color,
            background,
            alignment,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        }),
    };

    scene.set_tile_root(tile_id, node)?;

    Ok(SetContentResult {
        tile_id: tile_id.to_string(),
        node_id: node_id.to_string(),
        content_len: p.content.len(),
    })
}

// ─── dismiss ─────────────────────────────────────────────────────────────────

/// Parameters for `dismiss`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DismissParams {
    /// ID of the tile to delete. The tile's lease is revoked and the tile
    /// (plus all its nodes) is removed from the scene.
    pub tile_id: String,
}

/// Response from `dismiss`.
#[derive(Debug, Serialize)]
pub struct DismissResult {
    /// UUID of the tile that was dismissed.
    pub tile_id: String,
}

/// Delete a tile and release its lease.
///
/// Revokes the lease associated with the tile, which removes the tile and all
/// of its nodes from the scene. This is the inverse of `create_tile`.
///
/// # Errors
/// - `invalid_id` if `tile_id` is not a valid UUID.
/// - `scene_error` if the tile does not exist or its lease is not found.
pub fn handle_dismiss(params: Value, scene: &mut SceneGraph) -> McpResult<DismissResult> {
    let p: DismissParams = parse_params(params)?;
    let tile_id = parse_scene_id(&p.tile_id)?;

    let lease_id = scene
        .tiles
        .get(&tile_id)
        .ok_or_else(|| McpError::SceneError(format!("tile not found: {tile_id}")))?
        .lease_id;

    scene.revoke_lease(lease_id)?;

    Ok(DismissResult { tile_id: p.tile_id })
}

// ─── publish_to_zone ─────────────────────────────────────────────────────────

/// Parameters for `publish_to_zone`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PublishToZoneParams {
    /// Name of the target zone (must exist in the zone registry).
    pub zone_name: String,
    /// Content to publish. Accepts either:
    /// - A plain string → interpreted as `StreamText`
    /// - A JSON object with a `"type"` field → dispatched to the matching
    ///   `ZoneContent` variant:
    ///   - `{"type":"stream_text","text":"..."}` → `StreamText`
    ///   - `{"type":"notification","text":"...","icon":"","urgency":1}` → `Notification`
    ///   - `{"type":"status_bar","entries":{"key":"val",...}}` → `StatusBar`
    ///   - `{"type":"solid_color","r":1.0,"g":0.0,"b":0.0,"a":1.0}` → `SolidColor`
    ///   - `{"type":"static_image","resource_id":"<hex>"}` → `StaticImage`
    pub content: Value,
    /// Optional namespace for the lease. Defaults to "mcp".
    #[serde(default = "default_mcp_namespace")]
    pub namespace: String,
    /// Font size in pixels. Defaults to 16.
    #[serde(default = "default_font_size")]
    pub font_size_px: f32,
    /// TTL in microseconds. A value of 0 selects the built-in default of
    /// 60_000 ms (60_000_000 µs). Defaults to 0.
    #[serde(default)]
    pub ttl_us: u64,
    /// Merge key for idempotent zone publishes (optional).
    #[serde(default)]
    pub merge_key: Option<String>,
    /// Byte-offset breakpoints for streaming word-by-word reveal (optional).
    ///
    /// Only meaningful when `content` is a `StreamText` publish (plain string or
    /// `{"type":"stream_text","text":"..."}`).  Breakpoints identify byte offsets
    /// in the UTF-8 text string at which the compositor pauses reveal.
    ///
    /// An empty array (the default) reveals the full text immediately.
    ///
    /// Per spec §Subtitle Streaming Word-by-Word Reveal:
    ///   - `[3, 9, 15]` for `"The quick brown"` → reveals "The", "The quick",
    ///     "The quick brown", then the full text.
    ///
    /// `u64` is used for platform-stable JSON serialization (rather than
    /// `usize`, which is architecture-dependent).
    #[serde(default)]
    pub breakpoints: Vec<u64>,
}

/// Parse the polymorphic `content` field into a `ZoneContent`.
///
/// - Plain string → `StreamText`
/// - Object with `"type"` → dispatched by variant name
fn parse_zone_content(content: &Value) -> Result<ZoneContent, McpError> {
    match content {
        Value::String(s) => Ok(ZoneContent::StreamText(s.clone())),
        Value::Object(obj) => {
            let type_str = obj
                .get("type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    McpError::InvalidParams(
                        "object content must have a \"type\" field (one of: stream_text, notification, status_bar, solid_color, static_image)".to_string(),
                    )
                })?;
            match type_str {
                "stream_text" => {
                    let text = obj
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    Ok(ZoneContent::StreamText(text))
                }
                "notification" => {
                    let text = obj
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let icon = obj
                        .get("icon")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let urgency = obj.get("urgency").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                    let ttl_ms = obj.get("ttl_ms").and_then(|v| v.as_u64());
                    let title = obj
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let actions: Vec<NotificationAction> = match obj.get("actions") {
                        None => Vec::new(),
                        Some(Value::Array(arr)) => arr
                            .iter()
                            .enumerate()
                            .map(|(index, item)| {
                                let o = item.as_object().ok_or_else(|| {
                                    McpError::InvalidParams(format!(
                                        "notification.actions[{index}] must be an object"
                                    ))
                                })?;
                                let label = o
                                    .get("label")
                                    .and_then(|v| v.as_str())
                                    .filter(|v| !v.is_empty())
                                    .ok_or_else(|| {
                                        McpError::InvalidParams(format!(
                                            "notification.actions[{index}].label must be a non-empty string"
                                        ))
                                    })?
                                    .to_string();
                                let callback_id = o
                                    .get("callback_id")
                                    .and_then(|v| v.as_str())
                                    .filter(|v| !v.is_empty())
                                    .ok_or_else(|| {
                                        McpError::InvalidParams(format!(
                                            "notification.actions[{index}].callback_id must be a non-empty string"
                                        ))
                                    })?
                                    .to_string();
                                Ok(NotificationAction { label, callback_id })
                            })
                            .collect::<Result<Vec<_>, McpError>>()?,
                        Some(_) => {
                            return Err(McpError::InvalidParams(
                                "notification.actions must be an array".to_string(),
                            ))
                        }
                    };
                    Ok(ZoneContent::Notification(NotificationPayload {
                        text,
                        icon,
                        urgency,
                        ttl_ms,
                        title,
                        actions,
                    }))
                }
                "status_bar" => {
                    let entries: HashMap<String, String> = obj
                        .get("entries")
                        .and_then(|v| v.as_object())
                        .map(|m| {
                            m.iter()
                                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                                .collect()
                        })
                        .unwrap_or_default();
                    if entries.is_empty() {
                        return Err(McpError::InvalidParams(
                            "status_bar content must have a non-empty \"entries\" object"
                                .to_string(),
                        ));
                    }
                    Ok(ZoneContent::StatusBar(StatusBarPayload { entries }))
                }
                "solid_color" => {
                    let r = obj.get("r").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    let g = obj.get("g").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    let b = obj.get("b").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    let a = obj.get("a").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
                    Ok(ZoneContent::SolidColor(Rgba { r, g, b, a }))
                }
                "static_image" => {
                    use tze_hud_scene::types::ResourceId;
                    let hex = obj
                        .get("resource_id")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            McpError::InvalidParams(
                                "static_image content must have a \"resource_id\" field (hex-encoded 32-byte BLAKE3 hash)".to_string(),
                            )
                        })?;
                    // Decode hex without an external crate: parse pairs of chars as u8.
                    if hex.len() != 64 {
                        return Err(McpError::InvalidParams(format!(
                            "static_image \"resource_id\" must be 64 hex chars (32 bytes), got {}",
                            hex.len()
                        )));
                    }
                    let mut raw = [0u8; 32];
                    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
                        let hi = char::from(chunk[0]).to_digit(16);
                        let lo = char::from(chunk[1]).to_digit(16);
                        if let (Some(hi), Some(lo)) = (hi, lo) {
                            raw[i] = (hi * 16 + lo) as u8;
                        } else {
                            return Err(McpError::InvalidParams(format!(
                                "static_image \"resource_id\" is not valid hex: \"{hex}\""
                            )));
                        }
                    }
                    Ok(ZoneContent::StaticImage(ResourceId::from_bytes(raw)))
                }
                other => Err(McpError::InvalidParams(format!(
                    "unknown content type \"{other}\"; expected one of: stream_text, notification, status_bar, solid_color, static_image"
                ))),
            }
        }
        _ => Err(McpError::InvalidParams(
            "content must be a string or an object with a \"type\" field".to_string(),
        )),
    }
}

fn default_mcp_namespace() -> String {
    "mcp".to_string()
}

/// Response from `publish_to_zone`.
#[derive(Debug, Serialize)]
pub struct PublishToZoneResult {
    /// The zone name content was published to.
    pub zone_name: String,
    /// Effective TTL in microseconds applied to the lease (never 0 in response).
    pub ttl_us: u64,
    /// Echo of the merge key, if provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_key: Option<String>,
}

/// Publish markdown content to a named zone.
///
/// This is the primary LLM-first tool: a single call with zero scene context
/// required. It looks up the zone by name, grants a lease, and delegates to
/// the zone publishing engine (`SceneGraph::publish_to_zone_with_lease`), which
/// enforces contention policies, validates media types, respects
/// `geometry_policy`, and stores the publication in `zone_registry.active_publishes`.
/// Tile creation is deferred to the compositor, which resolves zone publishes
/// to tiles at render time.
///
/// Zone publishes are global (not tab-scoped in v1). No active tab is required.
///
/// # Errors
/// - `invalid_params` if `zone_name` or `content` is empty.
/// - `zone_not_found` if the zone name is not registered.
/// - `scene_error` for contention policy violations (max publishers, max keys)
///   or lease enforcement failures (no active lease, orphaned/suspended lease).
pub fn handle_publish_to_zone(
    params: Value,
    scene: &mut SceneGraph,
) -> McpResult<PublishToZoneResult> {
    let p: PublishToZoneParams = parse_params(params)?;

    if p.zone_name.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "zone_name must be non-empty".to_string(),
        ));
    }
    if p.content.is_null()
        || (p.content.is_string() && p.content.as_str().unwrap_or_default().is_empty())
    {
        return Err(McpError::InvalidParams(
            "content must be non-empty".to_string(),
        ));
    }

    // Validate zone exists before granting a lease, to fail fast on bad zone names.
    if !scene.zone_registry.zones.contains_key(&p.zone_name) {
        return Err(McpError::ZoneNotFound(p.zone_name));
    }

    // Parse the polymorphic content field into ZoneContent.
    let content = parse_zone_content(&p.content)?;

    // Convert ttl_us to ttl_ms for lease grant; 0 means use a sensible default.
    // Use div_ceil to ensure any positive sub-millisecond TTL rounds up to at
    // least 1 ms, preventing an unintended indefinite lease (ttl_ms == 0).
    let ttl_ms = if p.ttl_us == 0 {
        60_000u64 // 1 minute default
    } else {
        p.ttl_us.div_ceil(1_000)
    };

    // Content-level TTL for the zone publish record (distinct from the lease
    // TTL above). A zero `ttl_us` means "no content expiry" — the publication
    // persists until overwritten — so we pass `None`; a positive value becomes
    // an absolute `expires_at_wall_us` inside the publish engine. Without this,
    // `ttl_us` only bounded the lease and expired content stayed painted
    // indefinitely (hud-vfwb1).
    let content_ttl_us = if p.ttl_us == 0 { None } else { Some(p.ttl_us) };

    // Grant lease for MCP session tracking. Zone publishing requires an active
    // lease (spec §Zone Publish Requires Active Lease); we grant one here so
    // that publish_to_zone_with_lease can verify it.
    // We grant PublishZone(<zone_name>) — the zone-specific capability required
    // by the spec (§Capability Vocabulary: publish_zone:<zone_name>). The
    // tile/node capabilities previously granted here are vestigial; tile
    // creation is now deferred to the compositor and never done by this handler.
    let _lease_id = scene.grant_lease(
        &p.namespace,
        ttl_ms,
        vec![Capability::PublishZone(p.zone_name.clone())],
    );

    // Validate that breakpoints are only used with StreamText content.
    // Breakpoints are a StreamText-specific feature; sending them alongside
    // other content types is a caller error.
    if !p.breakpoints.is_empty() && !matches!(content, ZoneContent::StreamText(_)) {
        return Err(McpError::InvalidParams(
            "breakpoints are only valid for StreamText content".to_string(),
        ));
    }

    // Delegate to the real zone engine. This enforces contention policy
    // (LatestWins / Stack / MergeByKey), validates accepted_media_types,
    // and stores the record in zone_registry.active_publishes.
    //
    // When breakpoints are provided (StreamText streaming reveal), use the
    // breakpoint-aware variant so the compositor can reveal text progressively.
    if !p.breakpoints.is_empty() {
        scene.publish_to_zone_with_lease_and_breakpoints(
            &p.zone_name,
            content,
            &p.namespace,
            p.merge_key.clone(),
            content_ttl_us,
            p.breakpoints,
        )?;
    } else {
        scene.publish_to_zone_with_lease(
            &p.zone_name,
            content,
            &p.namespace,
            p.merge_key.clone(),
            content_ttl_us,
        )?;
    }

    Ok(PublishToZoneResult {
        zone_name: p.zone_name,
        // Return the effective TTL used for the lease (ttl_ms converted back to
        // microseconds), not the raw request value, so callers know what was applied.
        ttl_us: ttl_ms * 1_000,
        merge_key: p.merge_key,
    })
}

// ─── list_zones ──────────────────────────────────────────────────────────────

/// Parameters for `list_zones` — no required fields.
#[derive(Debug, Deserialize, Default, JsonSchema)]
pub struct ListZonesParams {}

/// A single zone entry in the list response.
#[derive(Debug, Serialize)]
pub struct ZoneEntry {
    /// Unique name of the zone.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Stable UUID for the zone definition.
    pub id: String,
    /// Whether the zone currently has any active publications (from `zone_registry.active_publishes`).
    /// This reflects occupancy from publish records, not tile visibility on the active tab.
    pub has_content: bool,
    /// Contention policy for this zone (e.g., "latest_wins", "stack", "merge_by_key", "replace").
    pub contention_policy: String,
    /// Media types accepted by this zone (e.g., ["stream_text", "notification"]).
    pub accepted_media_types: Vec<String>,
}

/// Response from `list_zones`.
#[derive(Debug, Serialize)]
pub struct ListZonesResult {
    /// All registered zones.
    pub zones: Vec<ZoneEntry>,
    /// Total number of zones.
    pub count: usize,
}

/// List all available zones and their current state.
///
/// `has_content` is true when `zone_registry.active_publishes` contains at
/// least one record for the zone — i.e., something has been published to the
/// zone and the record has not been evicted by contention policy or expiry.
/// This is the authoritative occupancy check, not a tile-namespace heuristic.
///
/// # Errors
/// - None (always succeeds; returns an empty list if no zones are registered).
pub fn handle_list_zones(params: Value, scene: &SceneGraph) -> McpResult<ListZonesResult> {
    // Use the same parse_params helper as other tool handlers; tolerates null → {}
    let _: ListZonesParams = parse_params(params)?;

    let mut zones: Vec<ZoneEntry> = scene
        .zone_registry
        .zones
        .values()
        .map(|z| {
            use tze_hud_scene::types::{ContentionPolicy, ZoneMediaType};

            let contention_policy = match z.contention_policy {
                ContentionPolicy::LatestWins => "latest_wins".to_string(),
                ContentionPolicy::Stack { .. } => "stack".to_string(),
                ContentionPolicy::MergeByKey { .. } => "merge_by_key".to_string(),
                ContentionPolicy::Replace => "replace".to_string(),
            };

            let accepted_media_types = z
                .accepted_media_types
                .iter()
                .map(|mt| match mt {
                    ZoneMediaType::StreamText => "stream_text".to_string(),
                    ZoneMediaType::ShortTextWithIcon => "notification".to_string(),
                    ZoneMediaType::KeyValuePairs => "status_bar".to_string(),
                    ZoneMediaType::StaticImage => "static_image".to_string(),
                    ZoneMediaType::SolidColor => "solid_color".to_string(),
                })
                .collect();

            ZoneEntry {
                name: z.name.clone(),
                description: z.description.clone(),
                id: z.id.to_string(),
                // A zone has content when zone_registry.active_publishes contains
                // at least one record for it. This is the authoritative source of
                // zone occupancy (not a tile-namespace heuristic).
                has_content: scene
                    .zone_registry
                    .active_publishes
                    .get(&z.name)
                    .is_some_and(|v| !v.is_empty()),
                contention_policy,
                accepted_media_types,
            }
        })
        .collect();

    // Stable ordering by name for deterministic output
    zones.sort_by(|a, b| a.name.cmp(&b.name));
    let count = zones.len();

    Ok(ListZonesResult { zones, count })
}

// ─── list_scene ──────────────────────────────────────────────────────────────

/// Parameters for `list_scene` — no required fields.
#[derive(Debug, Deserialize, Default, JsonSchema)]
pub struct ListSceneParams {}

/// A single tab entry in the list_scene response.
#[derive(Debug, Serialize)]
pub struct TabEntry {
    /// UUID of the tab.
    pub tab_id: String,
    /// Human-readable tab name.
    pub name: String,
    /// Display order.
    pub display_order: u32,
}

/// Response from `list_scene` (guest-restricted view).
///
/// Returns tab names and the zone registry only — not full tile topology.
/// This is intentionally limited to prevent guest agents from enumerating
/// the internal scene structure. Full topology is available to resident agents
/// via gRPC subscriptions.
#[derive(Debug, Serialize)]
pub struct ListSceneResult {
    /// All tabs in display order.
    pub tabs: Vec<TabEntry>,
    /// All registered zones (same as `list_zones`).
    pub zones: Vec<ZoneEntry>,
}

/// Return a restricted scene view: tab names and zone registry.
///
/// This is the guest-accessible variant of scene introspection. It does not
/// expose tile topology, node contents, lease state, or agent namespaces.
///
/// # Errors
/// - None (always succeeds; returns empty lists if scene is empty).
pub fn handle_list_scene(params: Value, scene: &SceneGraph) -> McpResult<ListSceneResult> {
    let _: ListSceneParams = parse_params(params)?;

    let mut tabs: Vec<TabEntry> = scene
        .tabs
        .values()
        .map(|t| TabEntry {
            tab_id: t.id.to_string(),
            name: t.name.clone(),
            display_order: t.display_order,
        })
        .collect();
    tabs.sort_by_key(|t| t.display_order);

    // Reuse list_zones logic for the zone portion
    let zones_result = handle_list_zones(Value::Null, scene)?;

    Ok(ListSceneResult {
        tabs,
        zones: zones_result.zones,
    })
}

// ─── list_elements ──────────────────────────────────────────────────────────

/// Parameters for `list_elements`.
#[derive(Debug, Deserialize, Default, JsonSchema)]
pub struct ListElementsParams {
    /// Optional namespace prefix filter.
    #[serde(default)]
    pub namespace_filter: Option<String>,
    /// Optional element type filter (`tile`, `zone`, or `widget`).
    #[serde(default)]
    pub element_type: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElementTypeFilter {
    Tile,
    Zone,
    Widget,
}

/// Relative geometry payload in the MCP list-elements response.
#[derive(Debug, Serialize)]
pub struct ElementGeometry {
    pub x_pct: f32,
    pub y_pct: f32,
    pub width_pct: f32,
    pub height_pct: f32,
}

/// A single element entry returned by `list_elements`.
#[derive(Debug, Serialize)]
pub struct ElementEntry {
    pub element_id: String,
    pub element_type: String,
    pub namespace: String,
    pub current_geometry: ElementGeometry,
    pub has_user_override: bool,
    pub created_at_ms: u64,
    pub last_published_at_ms: u64,
}

/// Response payload for `list_elements`.
#[derive(Debug, Serialize)]
pub struct ListElementsResult {
    pub elements: Vec<ElementEntry>,
    pub count: usize,
}

fn parse_element_type_filter(filter: Option<&str>) -> McpResult<Option<ElementTypeFilter>> {
    let Some(raw) = filter else {
        return Ok(None);
    };
    let normalized = raw.trim();
    if normalized.is_empty() {
        return Ok(None);
    }
    match normalized.to_ascii_lowercase().as_str() {
        "tile" => Ok(Some(ElementTypeFilter::Tile)),
        "zone" => Ok(Some(ElementTypeFilter::Zone)),
        "widget" => Ok(Some(ElementTypeFilter::Widget)),
        _ => Err(McpError::InvalidParams(
            "element_type must be one of: tile, zone, widget".to_string(),
        )),
    }
}

fn zero_geometry() -> ElementGeometry {
    ElementGeometry {
        x_pct: 0.0,
        y_pct: 0.0,
        width_pct: 0.0,
        height_pct: 0.0,
    }
}

fn geometry_policy_to_relative_entry(
    policy: GeometryPolicy,
    display_area: Rect,
) -> ElementGeometry {
    let relative = match policy {
        GeometryPolicy::Relative {
            x_pct,
            y_pct,
            width_pct,
            height_pct,
        } => {
            return ElementGeometry {
                x_pct,
                y_pct,
                width_pct,
                height_pct,
            };
        }
        _ => rect_to_relative_geometry_policy(
            geometry_policy_to_absolute_rect(policy, display_area.width, display_area.height),
            display_area.width,
            display_area.height,
        ),
    };

    match relative {
        GeometryPolicy::Relative {
            x_pct,
            y_pct,
            width_pct,
            height_pct,
        } => ElementGeometry {
            x_pct,
            y_pct,
            width_pct,
            height_pct,
        },
        _ => zero_geometry(),
    }
}

/// List all known scene elements with optional namespace/type filtering.
///
/// This MCP bridge view is scene-derived: it reports IDs and current geometry for
/// tiles, zone definitions, and widget instances.
pub fn handle_list_elements(params: Value, scene: &SceneGraph) -> McpResult<ListElementsResult> {
    let p: ListElementsParams = parse_params(params)?;
    let namespace_filter = p.namespace_filter.unwrap_or_default();
    let type_filter = parse_element_type_filter(p.element_type.as_deref())?;
    let display_area = scene.display_area;

    let mut elements = Vec::new();

    if type_filter.is_none() || type_filter == Some(ElementTypeFilter::Tile) {
        for tile in scene.tiles.values() {
            if !namespace_filter.is_empty() && !tile.namespace.starts_with(&namespace_filter) {
                continue;
            }
            let geometry = geometry_policy_to_relative_entry(
                rect_to_relative_geometry_policy(
                    tile.bounds,
                    display_area.width,
                    display_area.height,
                ),
                display_area,
            );
            elements.push(ElementEntry {
                element_id: tile.id.to_string(),
                element_type: "tile".to_string(),
                namespace: tile.namespace.clone(),
                current_geometry: geometry,
                has_user_override: false,
                created_at_ms: 0,
                last_published_at_ms: 0,
            });
        }
    }

    if type_filter.is_none() || type_filter == Some(ElementTypeFilter::Zone) {
        for zone in scene.zone_registry.zones.values() {
            if !namespace_filter.is_empty() && !zone.name.starts_with(&namespace_filter) {
                continue;
            }
            let geometry = scene
                .zone_registry
                .resolve_geometry_policy_for_zone(&zone.name, None, None)
                .map(|policy| geometry_policy_to_relative_entry(policy, display_area))
                .unwrap_or_else(zero_geometry);
            let last_published_at_ms = scene
                .zone_registry
                .active_for_zone(&zone.name)
                .iter()
                .map(|record| record.published_at_wall_us / 1_000)
                .max()
                .unwrap_or(0);
            elements.push(ElementEntry {
                element_id: zone.id.to_string(),
                element_type: "zone".to_string(),
                namespace: zone.name.clone(),
                current_geometry: geometry,
                has_user_override: false,
                created_at_ms: 0,
                last_published_at_ms,
            });
        }
    }

    if type_filter.is_none() || type_filter == Some(ElementTypeFilter::Widget) {
        for instance in scene.widget_registry.instances.values() {
            if !namespace_filter.is_empty()
                && !instance.instance_name.starts_with(&namespace_filter)
            {
                continue;
            }
            let geometry = scene
                .widget_registry
                .resolve_geometry_policy_for_instance(&instance.instance_name, None)
                .map(|policy| geometry_policy_to_relative_entry(policy, display_area))
                .unwrap_or_else(zero_geometry);
            let last_published_at_ms = scene
                .widget_registry
                .active_for_widget(&instance.instance_name)
                .iter()
                .map(|record| record.published_at_wall_us / 1_000)
                .max()
                .unwrap_or(0);
            elements.push(ElementEntry {
                element_id: instance.id.to_string(),
                element_type: "widget".to_string(),
                namespace: instance.instance_name.clone(),
                current_geometry: geometry,
                has_user_override: false,
                created_at_ms: 0,
                last_published_at_ms,
            });
        }
    }

    elements.sort_by(|a, b| {
        a.element_type
            .cmp(&b.element_type)
            .then_with(|| a.namespace.cmp(&b.namespace))
            .then_with(|| a.element_id.cmp(&b.element_id))
    });

    Ok(ListElementsResult {
        count: elements.len(),
        elements,
    })
}

// ─── publish_to_element ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum ElementTarget {
    Tile(SceneId),
    Zone(String),
    Widget(String),
}

fn resolve_element_target_by_id(scene: &SceneGraph, element_id: SceneId) -> Option<ElementTarget> {
    if scene.tiles.contains_key(&element_id) {
        return Some(ElementTarget::Tile(element_id));
    }
    if let Some(zone) = scene
        .zone_registry
        .zones
        .values()
        .find(|zone| zone.id == element_id)
    {
        return Some(ElementTarget::Zone(zone.name.clone()));
    }
    scene
        .widget_registry
        .instances
        .values()
        .find(|instance| instance.id == element_id)
        .map(|instance| ElementTarget::Widget(instance.instance_name.clone()))
}

/// Parameters for `publish_to_element`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PublishToElementParams {
    /// Stable element UUID.
    pub element_id: String,
    /// Content payload. For tiles and zones this follows the same content rules
    /// as `set_content` / `publish_to_zone`. For widgets this may be either an
    /// object of parameter key/value pairs or `{"params": {...}}`.
    pub content: Value,
    /// Optional namespace used for zone/widget publish bookkeeping.
    #[serde(default = "default_mcp_namespace")]
    pub namespace: String,
    /// Optional zone merge key.
    #[serde(default)]
    pub merge_key: Option<String>,
    /// Optional zone breakpoints for stream_text content.
    #[serde(default)]
    pub breakpoints: Option<Vec<u64>>,
    /// Optional zone/widget TTL (microseconds).
    #[serde(default)]
    pub ttl_us: Option<u64>,
    /// Optional widget transition duration.
    #[serde(default)]
    pub transition_ms: u32,
    /// Optional tile text style overrides.
    #[serde(default)]
    pub font_size_px: Option<f32>,
    #[serde(default)]
    pub color: Option<ColorParams>,
    #[serde(default)]
    pub background: Option<ColorParams>,
    #[serde(default)]
    pub alignment: Option<String>,
}

/// Response payload for `publish_to_element`.
#[derive(Debug, Serialize)]
pub struct PublishToElementResult {
    pub element_id: String,
    pub element_type: String,
    pub namespace: String,
    pub details: Value,
}

/// Publish content to an existing element ID (tile, zone, or widget).
pub fn handle_publish_to_element(
    params: Value,
    scene: &mut SceneGraph,
    caller_capabilities: &[String],
) -> McpResult<PublishToElementResult> {
    let p: PublishToElementParams = parse_params(params)?;
    let element_id = parse_scene_id(&p.element_id)?;
    let target = resolve_element_target_by_id(scene, element_id).ok_or_else(|| {
        McpError::SceneError(format!(
            "ELEMENT_NOT_FOUND: no tile, zone, or widget for element_id {}",
            p.element_id
        ))
    })?;

    match target {
        ElementTarget::Tile(tile_id) => {
            let text = match &p.content {
                Value::String(s) if !s.is_empty() => s.clone(),
                Value::Object(obj) => obj
                    .get("text")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        McpError::InvalidParams(
                            "tile publish content must be a non-empty string or object with non-empty text".to_string(),
                        )
                    })?,
                _ => {
                    return Err(McpError::InvalidParams(
                        "tile publish content must be a non-empty string or object with non-empty text".to_string(),
                    ))
                }
            };

            let mut set_params = serde_json::Map::new();
            set_params.insert("tile_id".to_string(), Value::String(tile_id.to_string()));
            set_params.insert("content".to_string(), Value::String(text));
            if let Some(font_size_px) = p.font_size_px {
                set_params.insert("font_size_px".to_string(), serde_json::json!(font_size_px));
            }
            if let Some(color) = p.color {
                set_params.insert(
                    "color".to_string(),
                    serde_json::json!({"r": color.r, "g": color.g, "b": color.b, "a": color.a}),
                );
            }
            if let Some(background) = p.background {
                set_params.insert(
                    "background".to_string(),
                    serde_json::json!({"r": background.r, "g": background.g, "b": background.b, "a": background.a}),
                );
            }
            if let Some(alignment) = p.alignment {
                set_params.insert("alignment".to_string(), Value::String(alignment));
            }

            let set_result = handle_set_content(Value::Object(set_params), scene)?;
            let details =
                serde_json::to_value(set_result).map_err(|e| McpError::Internal(e.to_string()))?;
            Ok(PublishToElementResult {
                element_id: p.element_id,
                element_type: "tile".to_string(),
                namespace: scene
                    .tiles
                    .get(&tile_id)
                    .map(|tile| tile.namespace.clone())
                    .unwrap_or_default(),
                details,
            })
        }
        ElementTarget::Zone(zone_name) => {
            let mut zone_params = serde_json::Map::new();
            zone_params.insert("zone_name".to_string(), Value::String(zone_name.clone()));
            zone_params.insert("content".to_string(), p.content);
            zone_params.insert("namespace".to_string(), Value::String(p.namespace.clone()));
            if let Some(ttl_us) = p.ttl_us {
                zone_params.insert("ttl_us".to_string(), serde_json::json!(ttl_us));
            }
            if let Some(merge_key) = p.merge_key.clone() {
                zone_params.insert("merge_key".to_string(), Value::String(merge_key));
            }
            if let Some(breakpoints) = p.breakpoints.clone() {
                zone_params.insert("breakpoints".to_string(), serde_json::json!(breakpoints));
            }

            let zone_result = handle_publish_to_zone(Value::Object(zone_params), scene)?;
            let details =
                serde_json::to_value(zone_result).map_err(|e| McpError::Internal(e.to_string()))?;
            Ok(PublishToElementResult {
                element_id: p.element_id,
                element_type: "zone".to_string(),
                namespace: zone_name,
                details,
            })
        }
        ElementTarget::Widget(widget_name) => {
            let params_value = match p.content {
                Value::Object(obj) => obj
                    .get("params")
                    .cloned()
                    .unwrap_or(Value::Object(obj)),
                _ => {
                    return Err(McpError::InvalidParams(
                        "widget publish content must be an object of parameter values or {\"params\": {...}}".to_string(),
                    ))
                }
            };
            if !params_value.is_object() {
                return Err(McpError::InvalidParams(
                    "widget publish content.params must be an object".to_string(),
                ));
            }

            let mut widget_params = serde_json::Map::new();
            widget_params.insert(
                "widget_name".to_string(),
                Value::String(widget_name.clone()),
            );
            widget_params.insert("params".to_string(), params_value);
            widget_params.insert("namespace".to_string(), Value::String(p.namespace));
            widget_params.insert(
                "transition_ms".to_string(),
                serde_json::json!(p.transition_ms),
            );
            if let Some(ttl_us) = p.ttl_us {
                widget_params.insert("ttl_us".to_string(), serde_json::json!(ttl_us));
            }

            let widget_result =
                handle_publish_to_widget(Value::Object(widget_params), scene, caller_capabilities)?;
            let details = serde_json::to_value(widget_result)
                .map_err(|e| McpError::Internal(e.to_string()))?;
            Ok(PublishToElementResult {
                element_id: p.element_id,
                element_type: "widget".to_string(),
                namespace: widget_name,
                details,
            })
        }
    }
}

// ─── publish_to_widget ───────────────────────────────────────────────────────

/// In-memory registry for MCP `register_widget_asset` dedup checks.
///
/// This mirrors the session protocol's hash-based short-circuit semantics:
/// if the content hash is already known, metadata-only preflight succeeds
/// without payload transfer.
#[derive(Debug, Default)]
pub struct WidgetAssetRegistry {
    by_hash: HashMap<[u8; 32], RegisteredWidgetAsset>,
}

#[derive(Debug)]
struct RegisteredWidgetAsset {
    asset_handle: String,
}

impl WidgetAssetRegistry {
    const MAX_ENTRIES: usize = 4096;

    fn get_by_hash(&self, hash: &[u8; 32]) -> Option<&RegisteredWidgetAsset> {
        self.by_hash.get(hash)
    }

    fn upsert(&mut self, hash: [u8; 32], asset_handle: String) {
        if self.by_hash.is_empty() {
            // Reserve a small initial slab to avoid repeated tiny growth churn.
            self.by_hash.reserve(64);
        }
        self.by_hash
            .insert(hash, RegisteredWidgetAsset { asset_handle });
    }

    fn is_at_capacity(&self) -> bool {
        self.by_hash.len() >= Self::MAX_ENTRIES
    }
}

/// Parameters for `register_widget_asset`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegisterWidgetAssetParams {
    /// Widget type id to associate with this SVG asset.
    pub widget_type_id: String,
    /// SVG filename (must end with `.svg`).
    pub svg_filename: String,
    /// Lowercase/uppercase 64-char hex BLAKE3 hash of payload bytes.
    pub content_hash_blake3: String,
    /// Declared payload size in bytes.
    pub total_size_bytes: u64,
    /// Optional transport integrity checksum (CRC32C).
    #[serde(default)]
    pub transport_crc32c: Option<u32>,
    /// Optional payload bytes represented as UTF-8 text.
    ///
    /// For SVG this is the raw SVG XML text.
    #[serde(default)]
    pub payload: Option<String>,
    /// When true, execute metadata-only dedup preflight.
    #[serde(default)]
    pub metadata_only_preflight: bool,
}

/// Response from `register_widget_asset`.
///
/// Shape mirrors session-protocol `WidgetAssetRegisterResult`.
#[derive(Debug, Serialize)]
pub struct RegisterWidgetAssetResult {
    pub accepted: bool,
    pub widget_type_id: String,
    pub svg_filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_handle: Option<String>,
    pub was_deduplicated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Register/upload runtime widget SVG assets on the MCP plane.
///
/// Semantics mirror session protocol `WidgetAssetRegister`:
/// - metadata-only preflight dedup short-circuit
/// - hash verification against payload bytes
/// - optional CRC32C transport integrity check
/// - stable WidgetAssetRegisterResult-style error codes
pub fn handle_register_widget_asset(
    params: Value,
    registry: &mut WidgetAssetRegistry,
    caller_capabilities: &[String],
) -> McpResult<RegisterWidgetAssetResult> {
    let p: RegisterWidgetAssetParams = parse_params(params)?;
    let mut result = RegisterWidgetAssetResult {
        accepted: false,
        widget_type_id: p.widget_type_id.clone(),
        svg_filename: p.svg_filename.clone(),
        asset_handle: None,
        was_deduplicated: false,
        error_code: None,
        error_message: None,
    };

    // Capability gate (session-protocol code parity).
    if !caller_capabilities
        .iter()
        .any(|c| c == "register_widget_asset")
    {
        return Ok(widget_asset_error(
            result,
            "WIDGET_ASSET_CAPABILITY_MISSING",
            "missing capability 'register_widget_asset'",
        ));
    }

    if p.widget_type_id.trim().is_empty()
        || p.svg_filename.trim().is_empty()
        || !p.svg_filename.to_ascii_lowercase().ends_with(".svg")
    {
        return Ok(widget_asset_error(
            result,
            "WIDGET_ASSET_TYPE_INVALID",
            "widget_type_id and svg_filename (.svg) are required",
        ));
    }

    const MAX_WIDGET_ASSET_BYTES: u64 = 16 * 1024 * 1024;
    if p.total_size_bytes > MAX_WIDGET_ASSET_BYTES {
        return Ok(widget_asset_error(
            result,
            "WIDGET_ASSET_BUDGET_EXCEEDED",
            "total_size_bytes exceeds runtime widget asset limit",
        ));
    }

    let expected_hash = match parse_blake3_hex_32(&p.content_hash_blake3) {
        Ok(h) => h,
        Err(msg) => {
            return Ok(widget_asset_error(
                result,
                "WIDGET_ASSET_TYPE_INVALID",
                &msg,
            ));
        }
    };

    if let Some(existing) = registry.get_by_hash(&expected_hash) {
        result.accepted = true;
        result.was_deduplicated = true;
        result.asset_handle = Some(existing.asset_handle.clone());
        return Ok(result);
    }

    if p.metadata_only_preflight {
        return Ok(widget_asset_error(
            result,
            "WIDGET_ASSET_HASH_MISMATCH",
            "metadata preflight miss; payload required",
        ));
    }

    let payload = match p.payload {
        Some(s) => s.into_bytes(),
        None => {
            return Ok(widget_asset_error(
                result,
                "WIDGET_ASSET_HASH_MISMATCH",
                "payload required for unknown hash",
            ));
        }
    };

    if payload.len() as u64 != p.total_size_bytes {
        return Ok(widget_asset_error(
            result,
            "WIDGET_ASSET_HASH_MISMATCH",
            "payload size does not match total_size_bytes",
        ));
    }

    if let Some(expected_crc32c) = p.transport_crc32c {
        let computed = crc32c(&payload);
        if computed != expected_crc32c {
            return Ok(widget_asset_error(
                result,
                "WIDGET_ASSET_CHECKSUM_MISMATCH",
                "transport_crc32c mismatch",
            ));
        }
    }

    let computed_hash = blake3::hash(&payload);
    if *computed_hash.as_bytes() != expected_hash {
        return Ok(widget_asset_error(
            result,
            "WIDGET_ASSET_HASH_MISMATCH",
            "content_hash_blake3 mismatch",
        ));
    }

    if !is_valid_svg_payload(&payload) {
        return Ok(widget_asset_error(
            result,
            "WIDGET_ASSET_INVALID_SVG",
            "payload is not a valid SVG document with <svg> root",
        ));
    }

    if registry.is_at_capacity() {
        return Ok(widget_asset_error(
            result,
            "WIDGET_ASSET_BUDGET_EXCEEDED",
            "runtime widget asset registry entry limit exceeded",
        ));
    }

    let asset_handle = format!("widget-asset:{}", bytes_to_hex(&expected_hash));
    // The MCP preflight index retains only metadata. The canonical runtime
    // registration path owns the payload copy.
    registry.upsert(expected_hash, asset_handle.clone());
    result.accepted = true;
    result.was_deduplicated = false;
    result.asset_handle = Some(asset_handle);
    Ok(result)
}

/// Parameters for `publish_to_widget`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PublishToWidgetParams {
    /// Widget instance name (instance_id or widget_type_name for single-instance).
    pub widget_name: String,
    /// Optional disambiguation: explicit instance_id when multiple instances of
    /// the same type exist on a tab. When provided, overrides `widget_name` for
    /// instance resolution.
    #[serde(default)]
    pub instance_id: Option<String>,
    /// Parameter values to publish. Keys are parameter names, values are typed.
    ///
    /// JSON type mapping:
    /// - f32 parameter → JSON number
    /// - string parameter → JSON string
    /// - color parameter → JSON object `{"r": number, "g": number, "b": number, "a": number}` (Note: currently parsed as f32 [0.0, 1.0], alignment to u8 planned)
    /// - enum parameter → JSON string
    pub params: HashMap<String, Value>,
    /// Transition duration in milliseconds (0 = instant). Defaults to 0.
    #[serde(default)]
    pub transition_ms: u32,
    /// Optional namespace (auto-derived from "mcp" if omitted).
    #[serde(default = "default_mcp_namespace")]
    pub namespace: String,
    /// TTL in microseconds (0 = use widget instance default). Defaults to 0.
    #[serde(default)]
    pub ttl_us: u64,
}

/// Response from `publish_to_widget`.
#[derive(Debug, Serialize)]
pub struct PublishToWidgetResult {
    /// Widget instance name that was published to.
    pub widget_name: String,
    /// Whether the widget is durable (true) or ephemeral (false).
    pub durable: bool,
    /// Parameter names that were successfully applied.
    pub applied_params: Vec<String>,
}

/// Convert a JSON `Value` to a `WidgetParameterValue` for a given param type.
///
/// Returns `None` if the value cannot be coerced to the expected type.
fn json_to_widget_param_value(
    v: &Value,
    param_name: &str,
    scene: &SceneGraph,
    widget_name: &str,
) -> Result<(String, WidgetParameterValue), McpError> {
    use tze_hud_scene::types::WidgetParamType;

    // Look up the parameter declaration from the widget schema.
    let instance = scene
        .widget_registry
        .instances
        .get(widget_name)
        .ok_or_else(|| McpError::SceneError(format!("widget not found: {widget_name}")))?;

    let definition = scene
        .widget_registry
        .definitions
        .get(&instance.widget_type_name)
        .ok_or_else(|| {
            McpError::SceneError(format!(
                "widget type not found: {}",
                instance.widget_type_name
            ))
        })?;

    let decl = definition
        .parameter_schema
        .iter()
        .find(|d| d.name == param_name)
        .ok_or_else(|| {
            McpError::SceneError(format!(
                "parameter '{param_name}' is not declared in widget '{widget_name}' schema (WIDGET_UNKNOWN_PARAMETER)"
            ))
        })?;

    let typed_value = match decl.param_type {
        WidgetParamType::F32 => {
            let f = v.as_f64().ok_or_else(|| {
                McpError::SceneError(format!("parameter '{param_name}' must be a number (f32)"))
            })? as f32;
            WidgetParameterValue::F32(f)
        }
        WidgetParamType::String => {
            let s = v.as_str().ok_or_else(|| {
                McpError::SceneError(format!("parameter '{param_name}' must be a string"))
            })?;
            WidgetParameterValue::String(s.to_string())
        }
        WidgetParamType::Color => {
            let obj = v.as_object().ok_or_else(|| {
                McpError::SceneError(format!(
                    "parameter '{param_name}' must be a color object {{r, g, b, a}}"
                ))
            })?;
            let r = obj.get("r").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
            let g = obj.get("g").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
            let b = obj.get("b").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
            let a = obj.get("a").and_then(|x| x.as_f64()).unwrap_or(1.0) as f32;
            WidgetParameterValue::Color(Rgba { r, g, b, a })
        }
        WidgetParamType::Enum => {
            let s = v.as_str().ok_or_else(|| {
                McpError::SceneError(format!(
                    "parameter '{param_name}' must be a string (enum value)"
                ))
            })?;
            WidgetParameterValue::Enum(s.to_string())
        }
    };

    Ok((param_name.to_string(), typed_value))
}

/// Publish parameter values to a named widget instance.
///
/// This is the primary widget interaction tool. It requires the
/// `publish_widget:<widget_name>` capability on the calling session.
///
/// # Capability
///
/// The `publish_widget:<widget_name>` capability must be present in
/// `caller_capabilities`. If absent, the call is rejected with
/// `WIDGET_CAPABILITY_MISSING`.
///
/// # Parameter types
///
/// The `params` object maps parameter names to JSON values. The JSON type
/// must match the parameter's declared type in the widget schema:
/// - f32 → JSON number
/// - string → JSON string
/// - color → JSON object `{"r": u8, "g": u8, "b": u8, "a": u8}`
/// - enum → JSON string
///
/// # Errors
/// - `invalid_params` if `widget_name` is empty or params is missing.
/// - `scene_error` with `WIDGET_CAPABILITY_MISSING` if capability absent.
/// - `scene_error` with `WIDGET_NOT_FOUND` if widget instance unknown.
/// - `scene_error` with `WIDGET_UNKNOWN_PARAMETER` if param name not in schema.
/// - `scene_error` with `WIDGET_PARAMETER_TYPE_MISMATCH` if value type wrong.
/// - `scene_error` with `WIDGET_PARAMETER_INVALID_VALUE` if value invalid.
pub fn handle_publish_to_widget(
    params: Value,
    scene: &mut SceneGraph,
    caller_capabilities: &[String],
) -> McpResult<PublishToWidgetResult> {
    let p: PublishToWidgetParams = parse_params(params)?;

    if p.widget_name.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "widget_name must be non-empty".to_string(),
        ));
    }

    // Resolve instance name: instance_id overrides widget_name when present.
    let resolved_name = p
        .instance_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&p.widget_name);

    // ── Capability gate (spec §Requirement: Widget Publishing via MCP) ────────
    let required_cap = format!("publish_widget:{}", p.widget_name);
    let has_cap = caller_capabilities.iter().any(|c| c == &required_cap);

    if !has_cap {
        return Err(McpError::SceneError(format!(
            "WIDGET_CAPABILITY_MISSING: missing capability '{required_cap}'"
        )));
    }

    // ── Validate widget exists ────────────────────────────────────────────────
    if !scene.widget_registry.instances.contains_key(resolved_name) {
        return Err(McpError::SceneError(format!(
            "WIDGET_NOT_FOUND: widget instance '{resolved_name}' not found"
        )));
    }

    // ── Convert JSON params to WidgetParameterValue map ───────────────────────
    let mut typed_params: HashMap<String, WidgetParameterValue> = HashMap::new();
    for (param_name, json_val) in &p.params {
        let (name, value) = json_to_widget_param_value(json_val, param_name, scene, resolved_name)?;
        typed_params.insert(name, value);
    }

    let applied_param_names: Vec<String> = typed_params.keys().cloned().collect();

    // ── Apply via scene graph (validates schema + contention policy) ──────────
    let is_durable = scene
        .publish_to_widget(
            resolved_name,
            typed_params,
            &p.namespace,
            None, // merge_key not supported in MCP v1
            p.transition_ms,
            None, // expires_at_wall_us from ttl_us (TTL conversion deferred)
        )
        .map_err(|e| {
            // Map ValidationErrors to WIDGET_* error codes in the message
            use tze_hud_scene::ValidationError;
            match &e {
                ValidationError::WidgetNotFound { .. } => {
                    McpError::SceneError(format!("WIDGET_NOT_FOUND: {e}"))
                }
                ValidationError::WidgetUnknownParameter { .. } => {
                    McpError::SceneError(format!("WIDGET_UNKNOWN_PARAMETER: {e}"))
                }
                ValidationError::WidgetParameterTypeMismatch { .. } => {
                    McpError::SceneError(format!("WIDGET_PARAMETER_TYPE_MISMATCH: {e}"))
                }
                ValidationError::WidgetParameterInvalidValue { .. } => {
                    McpError::SceneError(format!("WIDGET_PARAMETER_INVALID_VALUE: {e}"))
                }
                ValidationError::WidgetCapabilityMissing { .. } => {
                    McpError::SceneError(format!("WIDGET_CAPABILITY_MISSING: {e}"))
                }
                _ => McpError::SceneError(e.to_string()),
            }
        })?;

    Ok(PublishToWidgetResult {
        widget_name: resolved_name.to_string(),
        durable: is_durable,
        applied_params: applied_param_names,
    })
}

// ─── list_widgets ─────────────────────────────────────────────────────────────

/// Parameters for `list_widgets` — no required fields.
#[derive(Debug, Deserialize, Default, JsonSchema)]
pub struct ListWidgetsParams {}

/// A parameter declaration entry in the list_widgets response.
#[derive(Debug, Serialize)]
pub struct WidgetParamEntry {
    /// Parameter name.
    pub name: String,
    /// Parameter type: "f32", "string", "color", or "enum".
    pub param_type: String,
    /// Constraints (present when non-default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub f32_min: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub f32_max: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub string_max_bytes: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub enum_allowed_values: Vec<String>,
}

/// A widget type entry in the list_widgets response.
#[derive(Debug, Serialize)]
pub struct WidgetTypeEntry {
    /// Unique widget type id (kebab-case).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Whether publishes to this widget type are fire-and-forget (no ack).
    pub ephemeral: bool,
    /// Parameter schema.
    pub parameter_schema: Vec<WidgetParamEntry>,
}

/// A widget instance entry in the list_widgets response.
#[derive(Debug, Serialize)]
pub struct WidgetInstanceEntry {
    /// Widget type name.
    pub widget_type: String,
    /// Instance addressing key.
    pub instance_name: String,
    /// Tab UUID this instance is bound to.
    pub tab_id: String,
    /// Current effective parameter values (from last publish or defaults).
    pub current_params: HashMap<String, Value>,
}

/// Response from `list_widgets`.
#[derive(Debug, Serialize)]
pub struct ListWidgetsResult {
    /// All registered widget types.
    pub widget_types: Vec<WidgetTypeEntry>,
    /// All widget instances with their current state.
    pub widget_instances: Vec<WidgetInstanceEntry>,
    /// Total widget type count.
    pub type_count: usize,
    /// Total instance count.
    pub instance_count: usize,
}

/// Convert a `WidgetParameterValue` to a JSON `Value` for the list_widgets response.
fn widget_param_value_to_json(v: &WidgetParameterValue) -> Value {
    match v {
        WidgetParameterValue::F32(f) => Value::from(*f as f64),
        WidgetParameterValue::String(s) => Value::String(s.clone()),
        WidgetParameterValue::Color(c) => serde_json::json!({
            "r": c.r, "g": c.g, "b": c.b, "a": c.a
        }),
        WidgetParameterValue::Enum(e) => Value::String(e.clone()),
    }
}

/// List all registered widget types and their instances with current parameter values.
///
/// Returns an empty result set if no widget bundles are configured. This tool
/// is guest-accessible and requires no special capability.
///
/// # Errors
/// - None (always succeeds; returns empty lists if registry is empty).
pub fn handle_list_widgets(params: Value, scene: &SceneGraph) -> McpResult<ListWidgetsResult> {
    let _: ListWidgetsParams = parse_params(params)?;

    // ── Widget types ─────────────────────────────────────────────────────────
    use tze_hud_scene::types::WidgetParamType;

    let mut widget_types: Vec<WidgetTypeEntry> = scene
        .widget_registry
        .definitions
        .values()
        .map(|def| {
            let parameter_schema = def
                .parameter_schema
                .iter()
                .map(|decl| {
                    let param_type = match decl.param_type {
                        WidgetParamType::F32 => "f32",
                        WidgetParamType::String => "string",
                        WidgetParamType::Color => "color",
                        WidgetParamType::Enum => "enum",
                    }
                    .to_string();
                    let (f32_min, f32_max, string_max_bytes, enum_allowed_values) =
                        if let Some(c) = &decl.constraints {
                            (
                                c.f32_min,
                                c.f32_max,
                                c.string_max_bytes,
                                c.enum_allowed_values.clone(),
                            )
                        } else {
                            (None, None, None, vec![])
                        };
                    WidgetParamEntry {
                        name: decl.name.clone(),
                        param_type,
                        f32_min,
                        f32_max,
                        string_max_bytes,
                        enum_allowed_values,
                    }
                })
                .collect();
            WidgetTypeEntry {
                id: def.id.clone(),
                name: def.name.clone(),
                description: def.description.clone(),
                ephemeral: def.ephemeral,
                parameter_schema,
            }
        })
        .collect();

    // Stable ordering by id
    widget_types.sort_by(|a, b| a.id.cmp(&b.id));

    // ── Widget instances ──────────────────────────────────────────────────────
    let mut widget_instances: Vec<WidgetInstanceEntry> = scene
        .widget_registry
        .instances
        .values()
        .map(|inst| {
            let current_params: HashMap<String, Value> = inst
                .current_params
                .iter()
                .map(|(k, v)| (k.clone(), widget_param_value_to_json(v)))
                .collect();
            WidgetInstanceEntry {
                widget_type: inst.widget_type_name.clone(),
                instance_name: inst.instance_name.clone(),
                tab_id: inst.tab_id.to_string(),
                current_params,
            }
        })
        .collect();

    // Stable ordering by instance_name
    widget_instances.sort_by(|a, b| a.instance_name.cmp(&b.instance_name));

    let type_count = widget_types.len();
    let instance_count = widget_instances.len();

    Ok(ListWidgetsResult {
        widget_types,
        widget_instances,
        type_count,
        instance_count,
    })
}

// ─── clear_widget ─────────────────────────────────────────────────────────────

/// Parameters for `clear_widget`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClearWidgetParams {
    /// Widget instance name (addressing key).
    pub widget_name: String,
    /// Agent namespace performing the clear. Defaults to "" (cleared publications
    /// belonging to the namespace are removed).
    #[serde(default)]
    pub namespace: String,
    /// Optional disambiguation when multiple instances share the same name.
    #[serde(default)]
    pub instance_id: Option<String>,
}

/// Response from `clear_widget`.
#[derive(Debug, Serialize)]
pub struct ClearWidgetResult {
    /// Resolved widget instance name.
    pub widget_name: String,
    /// True — the operation always succeeds or returns an error.
    pub cleared: bool,
    /// Whether the caller owned an active publication that was removed.
    pub changed: bool,
}

/// Clear all publications by the calling agent on the specified widget instance.
///
/// Mirrors `clear_zone` semantics: removes only the calling agent's publications.
/// If no publications exist for the publisher this is a no-op (still succeeds).
/// When all publishers have been cleared the widget reverts to its default params.
///
/// # Capability
///
/// The `publish_widget:<widget_name>` capability must be present in
/// `caller_capabilities`. Agents may only clear their own publications.
///
/// # Errors
/// - `invalid_params` if `widget_name` is empty.
/// - `scene_error` with `WIDGET_CAPABILITY_MISSING` if capability absent.
/// - `scene_error` with `WIDGET_NOT_FOUND` if widget instance unknown.
pub fn handle_clear_widget(
    params: Value,
    scene: &mut SceneGraph,
    caller_capabilities: &[String],
) -> McpResult<ClearWidgetResult> {
    let p: ClearWidgetParams = parse_params(params)?;

    if p.widget_name.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "widget_name must be non-empty".to_string(),
        ));
    }

    // Resolve instance name: instance_id overrides widget_name when present.
    let resolved_name = p
        .instance_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&p.widget_name);

    // ── Capability gate (mirrors publish_to_widget) ───────────────────────────
    let required_cap = format!("publish_widget:{}", p.widget_name);
    let has_cap = caller_capabilities.iter().any(|c| c == &required_cap);

    if !has_cap {
        return Err(McpError::SceneError(format!(
            "WIDGET_CAPABILITY_MISSING: missing capability '{required_cap}'"
        )));
    }

    // ── Delegate to scene graph ───────────────────────────────────────────────
    let scene_version_before = scene.version;
    scene
        .clear_widget_for_publisher(resolved_name, &p.namespace)
        .map_err(|e| {
            use tze_hud_scene::ValidationError;
            match &e {
                ValidationError::WidgetNotFound { .. } => {
                    McpError::SceneError(format!("WIDGET_NOT_FOUND: {e}"))
                }
                _ => McpError::SceneError(e.to_string()),
            }
        })?;

    Ok(ClearWidgetResult {
        widget_name: resolved_name.to_string(),
        cleared: true,
        changed: scene.version != scene_version_before,
    })
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn widget_asset_error(
    mut base: RegisterWidgetAssetResult,
    code: &str,
    message: &str,
) -> RegisterWidgetAssetResult {
    base.accepted = false;
    base.was_deduplicated = false;
    base.asset_handle = None;
    base.error_code = Some(code.to_string());
    base.error_message = Some(message.to_string());
    base
}

fn parse_blake3_hex_32(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 {
        return Err(format!(
            "content_hash_blake3 must be 64 hex chars, got {}",
            hex.len()
        ));
    }
    let mut raw = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = char::from(chunk[0]).to_digit(16);
        let lo = char::from(chunk[1]).to_digit(16);
        if let (Some(hi), Some(lo)) = (hi, lo) {
            raw[i] = ((hi << 4) | lo) as u8;
        } else {
            return Err("content_hash_blake3 is not valid hex".to_string());
        }
    }
    Ok(raw)
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn crc32c(bytes: &[u8]) -> u32 {
    // Castagnoli polynomial (reversed) used by CRC32C.
    const POLY: u32 = 0x82F63B78;
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (POLY & mask);
        }
    }
    !crc
}

fn is_valid_svg_payload(payload: &[u8]) -> bool {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_reader(payload);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut root_seen = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if !root_seen {
                    root_seen = true;
                    if e.local_name().as_ref() != b"svg" {
                        return false;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return false,
        }
        buf.clear();
    }
    root_seen
}

/// Deserialize tool parameters from a JSON value.
fn parse_params<T: for<'de> serde::Deserialize<'de>>(params: Value) -> McpResult<T> {
    // Treat null params as an empty object for tools with all-optional params
    let v = if params.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        params
    };
    serde_json::from_value(v).map_err(|e| McpError::InvalidParams(e.to_string()))
}

/// Parse a string as a [`SceneId`] (UUID).
fn parse_scene_id(s: &str) -> McpResult<SceneId> {
    uuid::Uuid::parse_str(s)
        .map(SceneId::from_uuid)
        .map_err(|e| McpError::InvalidId(format!("invalid UUID '{s}': {e}")))
}

// ─── inject_composer_paste ────────────────────────────────────────────────────

/// Parameters for `inject_composer_paste`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct InjectComposerPasteParams {
    /// Text to inject into the active composer draft buffer.
    ///
    /// CR, LF, and control characters are stripped by the runtime before
    /// insertion (spec §4.4). The runtime paste path enforces the draft cap
    /// (DEFAULT_DRAFT_CAP = 4096 bytes); excess is silently truncated at a
    /// grapheme-cluster boundary.
    pub text: String,
}

/// Response from `inject_composer_paste`.
#[derive(Debug, Serialize)]
pub struct InjectComposerPasteResult {
    /// Whether the text was injected into an active composer.
    /// `false` when no composer region is currently focused.
    pub injected: bool,
    /// Number of UTF-8 bytes in the text as received (before sanitisation).
    pub text_len: usize,
}

/// Inject text into the active composer draft buffer.
///
/// Forwards the text over the `paste_inject_tx` channel. The windowed runtime
/// drains the channel on each event-loop iteration and calls
/// `InputProcessor::inject_paste_to_composer`. If no channel is wired
/// (headless or channel not configured), returns `injected: false`.
///
/// # Errors
/// - `invalid_params` if `text` is empty.
pub fn handle_inject_composer_paste(
    params: serde_json::Value,
    paste_inject_tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
) -> crate::types::McpResult<InjectComposerPasteResult> {
    let p: InjectComposerPasteParams = parse_params(params)?;

    if p.text.is_empty() {
        return Err(crate::error::McpError::InvalidParams(
            "text must be non-empty".to_string(),
        ));
    }

    let text_len = p.text.len();

    let injected = if let Some(tx) = paste_inject_tx {
        tx.send(p.text).is_ok()
    } else {
        false
    };

    Ok(InjectComposerPasteResult { injected, text_len })
}

// ─── portal_projection_list ──────────────────────────────────────────────────

/// Parameters for `portal_projection_list`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PortalProjectionListParams {}

/// Content-free result from `portal_projection_list`.
#[derive(Debug, Serialize)]
pub struct PortalProjectionListResult {
    /// Bounded summaries owned by the resident caller.
    pub projections: Vec<crate::portal_op::ProjectionListEntry>,
}

/// List the resident caller's active projections for recovery or reconciliation.
///
/// The operation is read-only and returns no transcript text, pending-input
/// text, owner tokens, lease data, or other principals' sessions.
///
/// Side effects: sends one read-only request to the runtime authority channel.
/// State: current caller-scoped authority state; no client-held projection state.
/// Idempotency: yes — repeated calls return the current bounded summaries only.
/// Failure: internal when the authority channel is unavailable or dropped;
/// authority rejections retain their stable `PROJECTION_*` code.
/// Concurrency: awaits an independent one-shot reply; authority operations are
/// serialized by the runtime event loop.
pub async fn handle_portal_projection_list(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
) -> McpResult<PortalProjectionListResult> {
    let _: PortalProjectionListParams = parse_params(params)?;
    let tx = portal_op_tx.ok_or_else(|| {
        McpError::Internal(
            "portal authority not wired — runtime must enable portal_op channel".to_string(),
        )
    })?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::portal_op::PortalOp::List { reply: reply_tx })
        .map_err(|_| McpError::Internal("portal authority channel closed".to_string()))?;

    match reply_rx.await {
        Ok(Ok(batch)) => Ok(PortalProjectionListResult {
            projections: batch.projections,
        }),
        Ok(Err(rejection)) => Err(McpError::ProjectionRejected {
            error_code: rejection.error_code,
            operation: "portal_projection_list",
        }),
        Err(_) => Err(McpError::Internal(
            "portal authority did not respond (channel dropped)".to_string(),
        )),
    }
}

// ─── portal_projection_attach ────────────────────────────────────────────────

/// Parameters for `portal_projection_attach`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PortalProjectionAttachParams {
    /// Session id (max 128 bytes), unique among active projections. Reuse needs a matching `idempotency_key`.
    pub projection_id: String,
    /// Human-readable label (max 128 bytes).
    pub display_name: String,
    /// Optional key for replay-safe re-attach after an interruption. A matching
    /// authenticated replay returns a fresh owner token and invalidates the old
    /// token without extending the original expiry deadline.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Optional provider: `codex`|`claude`|`opencode`|`other` (default `other`).
    #[serde(default)]
    pub provider_kind: Option<String>,
    /// Optional classification: `public`|`household`|`private`|`sensitive` (default `private`).
    #[serde(default)]
    pub content_classification: Option<String>,
    /// Optional workspace hint (e.g. project directory).
    #[serde(default)]
    pub workspace_hint: Option<String>,
    /// Optional repository hint (e.g. repo URL or name).
    #[serde(default)]
    pub repository_hint: Option<String>,
    /// Optional icon profile hint for visual identity.
    #[serde(default)]
    pub icon_profile_hint: Option<String>,
    /// Optional HUD target hint for multi-display routing.
    #[serde(default)]
    pub hud_target: Option<String>,
}

/// Response from `portal_projection_attach`.
#[derive(Debug, Serialize)]
pub struct PortalProjectionAttachResult {
    /// `true` when the authority accepted the attach.
    pub accepted: bool,
    /// Owner token (only present on success). Required for
    /// subsequent `portal_projection_publish` calls.
    pub owner_token: Option<String>,
    /// Human-readable status summary.
    pub status_summary: String,
}

/// Attach a new projection session to the in-process authority (hud-bq0gl.2).
///
/// Forwards the request through the portal-op channel to the winit
/// event-loop thread.  On the next `about_to_wait` iteration the driver
/// calls `handle_attach` + `attach_projection` so the session is ready for
/// subsequent `portal_projection_publish` calls and drain-loop tile creation.
///
/// # Errors
///
/// - `invalid_params` if `projection_id` or `display_name` is empty.
/// - `internal` if the portal authority is not wired (runtime started without
///   the portal channel — call `McpServer::with_portal_op_tx` to enable this).
/// - `internal` if the authority rejected the attach (e.g., duplicate id).
pub async fn handle_portal_projection_attach(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
) -> McpResult<PortalProjectionAttachResult> {
    let render_wake = tze_hud_scene::render_wake::RenderWakeNotifier::default();
    handle_portal_projection_attach_with_render_wake(params, portal_op_tx, &render_wake).await
}

pub(crate) async fn handle_portal_projection_attach_with_render_wake(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) -> McpResult<PortalProjectionAttachResult> {
    let p: PortalProjectionAttachParams = parse_params(params)?;

    if p.projection_id.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "projection_id must be non-empty".to_string(),
        ));
    }
    if p.display_name.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "display_name must be non-empty".to_string(),
        ));
    }

    let tx = portal_op_tx.ok_or_else(|| {
        McpError::Internal(
            "portal authority not wired — runtime must enable portal_op channel".to_string(),
        )
    })?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::portal_op::PortalOp::Attach {
        projection_id: p.projection_id,
        display_name: p.display_name,
        idempotency_key: p.idempotency_key,
        provider_kind: p.provider_kind,
        content_classification: p.content_classification,
        workspace_hint: p.workspace_hint,
        repository_hint: p.repository_hint,
        icon_profile_hint: p.icon_profile_hint,
        hud_target: p.hud_target,
        reply: reply_tx,
    })
    .map_err(|_| McpError::Internal("portal authority channel closed".to_string()))?;
    render_wake.notify();

    match reply_rx.await {
        Ok(Ok(token)) => Ok(PortalProjectionAttachResult {
            accepted: true,
            owner_token: Some(token),
            status_summary: "projection attached".to_string(),
        }),
        Ok(Err(rejection)) => Err(McpError::ProjectionRejected {
            error_code: rejection.error_code,
            operation: "portal_projection_attach",
        }),
        Err(_) => Err(McpError::Internal(
            "portal authority did not respond (channel dropped)".to_string(),
        )),
    }
}

// ─── portal_projection_publish ───────────────────────────────────────────────

/// Parameters for `portal_projection_publish`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PortalProjectionPublishParams {
    /// Projection session id from attach.
    pub projection_id: String,
    /// Owner token from attach.
    pub owner_token: String,
    /// Text to append to the transcript.
    pub output_text: String,
    /// Optional logical-unit id for idempotent dedup (max 128 bytes).
    #[serde(default)]
    pub logical_unit_id: Option<String>,
    /// Optional kind: `assistant` (default)|`tool`|`status`|`error`|`other`.
    #[serde(default)]
    pub output_kind: Option<String>,
    /// Optional classification: `public`|`household`|`private`|`sensitive` (default `private`).
    #[serde(default)]
    pub content_classification: Option<String>,
    /// Optional key; repeated publishes with the same key collapse in-place instead of appending.
    #[serde(default)]
    pub coalesce_key: Option<String>,
    /// Optional; `true` marks this output as a question awaiting a viewer reply.
    #[serde(default)]
    pub expects_reply: Option<bool>,
}

/// Response from `portal_projection_publish`.
#[derive(Debug, Serialize)]
pub struct PortalProjectionPublishResult {
    /// `true` when the authority accepted and queued the output for rendering.
    pub accepted: bool,
    /// Human-readable status summary.
    pub status_summary: String,
}

/// Publish output text to an existing projection session (hud-bq0gl.2).
///
/// Forwards the request through the portal-op channel to the winit
/// event-loop thread.  On the next `about_to_wait` iteration the driver
/// calls `handle_publish_output`, which routes the content through the
/// cadence coalescer.  The drain loop then materialises the coalesced update
/// into the live scene, advancing follow-tail (spec §3.2) and preserving
/// scrolled-back stability (spec §3.3).
///
/// Per the cooperative-hud-projection contract (spec Requirement: Low-Token
/// LLM-Facing Operations), `publish_output` also accepts optional
/// `output_kind`, `content_classification`, and `coalesce_key`. These are
/// forwarded verbatim as snake_case strings; the runtime driver parses them
/// into the projection enums and defaults safely to `assistant` /
/// `private` / no-coalesce when omitted. Privacy stays safe-by-default: an
/// omitted classification is treated as `private`.
///
/// `expects_reply` (a.k.a. `Question`) is an optional bool signaling that this
/// output is a question awaiting a viewer reply. Omitted/`false` is the exact
/// pre-existing behavior (no rendered cue); `true` drives a minimal, ambient,
/// token-styled cue on the portal (hud-jip0k). Backward-compatible opt-in.
///
/// Accepted `output_kind` values (snake_case): `assistant` (default), `tool`,
/// `status`, `error`, `other`. Any other value is rejected with
/// `PROJECTION_INVALID_ARGUMENT`.
///
/// # Errors
///
/// - `invalid_params` if `projection_id`, `owner_token`, or `output_text` is empty.
/// - `internal` if the portal authority is not wired.
/// - `internal` if the authority rejected the publish (invalid token, rate limit,
///   oversized payload, unrecognized `output_kind` / `content_classification`, etc.).
pub async fn handle_portal_projection_publish(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
) -> McpResult<PortalProjectionPublishResult> {
    let render_wake = tze_hud_scene::render_wake::RenderWakeNotifier::default();
    handle_portal_projection_publish_with_render_wake(params, portal_op_tx, &render_wake).await
}

pub(crate) async fn handle_portal_projection_publish_with_render_wake(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) -> McpResult<PortalProjectionPublishResult> {
    let p: PortalProjectionPublishParams = parse_params(params)?;

    if p.projection_id.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "projection_id must be non-empty".to_string(),
        ));
    }
    if p.owner_token.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "owner_token must be non-empty".to_string(),
        ));
    }
    if p.output_text.is_empty() {
        return Err(McpError::InvalidParams(
            "output_text must be non-empty".to_string(),
        ));
    }

    let tx = portal_op_tx.ok_or_else(|| {
        McpError::Internal(
            "portal authority not wired — runtime must enable portal_op channel".to_string(),
        )
    })?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::portal_op::PortalOp::PublishOutput {
        projection_id: p.projection_id,
        owner_token: p.owner_token,
        output_text: p.output_text,
        logical_unit_id: p.logical_unit_id,
        output_kind: p.output_kind,
        content_classification: p.content_classification,
        coalesce_key: p.coalesce_key,
        expects_reply: p.expects_reply,
        reply: reply_tx,
    })
    .map_err(|_| McpError::Internal("portal authority channel closed".to_string()))?;
    render_wake.notify();

    match reply_rx.await {
        Ok(Ok(())) => Ok(PortalProjectionPublishResult {
            accepted: true,
            status_summary: "output queued for portal rendering".to_string(),
        }),
        Ok(Err(rejection)) => Err(McpError::ProjectionRejected {
            error_code: rejection.error_code,
            operation: "portal_projection_publish",
        }),
        Err(_) => Err(McpError::Internal(
            "portal authority did not respond (channel dropped)".to_string(),
        )),
    }
}

// ─── portal_projection_publish_status ────────────────────────────────────────

/// Parameters for `portal_projection_publish_status`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PortalProjectionPublishStatusParams {
    /// Projection session id from attach.
    pub projection_id: String,
    /// Owner token from attach.
    pub owner_token: String,
    /// Lifecycle: `attached`|`active`|`degraded`|`hud_unavailable`|`detached`|`cleanup_pending`|`expired`. No `waiting`/`blocked` — use `status_text`.
    pub lifecycle_state: String,
    /// Optional status detail (bounded by the authority's `max_status_text_bytes`).
    #[serde(default)]
    pub status_text: Option<String>,
}

/// Response from `portal_projection_publish_status`.
#[derive(Debug, Serialize)]
pub struct PortalProjectionPublishStatusResult {
    /// `true` when the authority accepted and applied the lifecycle state.
    pub accepted: bool,
    /// Human-readable status summary.
    pub status_summary: String,
    /// The applied lifecycle state echoed back as a snake_case string — the
    /// observable round-trip confirming the viewer-facing state the authority
    /// now holds for this projection.
    pub lifecycle_state: String,
}

/// Publish a lifecycle status to an existing projection session (hud-y8h3m).
///
/// This is step 3 of the cooperative projection workflow and the only way the
/// owning LLM signals its lifecycle state — `active`, `degraded` (blocked),
/// `attached` (waiting for input), etc. — to the viewer. The status drives
/// earned-urgency and ambient-attention affordances on the portal. It stays on
/// the existing MCP transport and is ambient, not interruptive.
///
/// Forwards the request through the portal-op channel to the winit event-loop
/// thread. On the next `about_to_wait` iteration the driver parses the
/// snake_case `lifecycle_state` into the projection enum and calls
/// `handle_publish_status`, which applies it to the session and echoes the
/// applied state back.
///
/// Accepted `lifecycle_state` values (snake_case): `attached`, `active`,
/// `degraded`, `hud_unavailable`, `detached`, `cleanup_pending`, `expired`.
/// Any other value is rejected with `PROJECTION_INVALID_ARGUMENT`.
///
/// # Errors
///
/// - `invalid_params` if `projection_id`, `owner_token`, or `lifecycle_state` is empty.
/// - `internal` if the portal authority is not wired.
/// - `ProjectionRejected` if the authority rejected the status (invalid / expired
///   token, unrecognized `lifecycle_state`, oversized `status_text`, etc.), carrying
///   the stable `PROJECTION_*` code in `error.data.error_code`.
pub async fn handle_portal_projection_publish_status(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
) -> McpResult<PortalProjectionPublishStatusResult> {
    let render_wake = tze_hud_scene::render_wake::RenderWakeNotifier::default();
    handle_portal_projection_publish_status_with_render_wake(params, portal_op_tx, &render_wake)
        .await
}

pub(crate) async fn handle_portal_projection_publish_status_with_render_wake(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) -> McpResult<PortalProjectionPublishStatusResult> {
    let p: PortalProjectionPublishStatusParams = parse_params(params)?;

    if p.projection_id.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "projection_id must be non-empty".to_string(),
        ));
    }
    if p.owner_token.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "owner_token must be non-empty".to_string(),
        ));
    }
    if p.lifecycle_state.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "lifecycle_state must be non-empty".to_string(),
        ));
    }

    let tx = portal_op_tx.ok_or_else(|| {
        McpError::Internal(
            "portal authority not wired — runtime must enable portal_op channel".to_string(),
        )
    })?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::portal_op::PortalOp::PublishStatus {
        projection_id: p.projection_id,
        owner_token: p.owner_token,
        lifecycle_state: p.lifecycle_state,
        status_text: p.status_text,
        reply: reply_tx,
    })
    .map_err(|_| McpError::Internal("portal authority channel closed".to_string()))?;
    render_wake.notify();

    match reply_rx.await {
        Ok(Ok(lifecycle_state)) => Ok(PortalProjectionPublishStatusResult {
            accepted: true,
            status_summary: "lifecycle status applied".to_string(),
            lifecycle_state,
        }),
        Ok(Err(rejection)) => Err(McpError::ProjectionRejected {
            error_code: rejection.error_code,
            operation: "portal_projection_publish_status",
        }),
        Err(_) => Err(McpError::Internal(
            "portal authority did not respond (channel dropped)".to_string(),
        )),
    }
}

// ─── portal_projection_get_pending_input ─────────────────────────────────────

/// Parameters for `portal_projection_get_pending_input`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PortalProjectionGetPendingInputParams {
    /// Projection session id from attach.
    pub projection_id: String,
    /// Owner token from attach.
    pub owner_token: String,
    /// Optional cap on items returned (clamped to the authority's `max_poll_items`).
    #[serde(default)]
    pub max_items: Option<usize>,
    /// Optional cap on response bytes (clamped to the authority's `max_poll_response_bytes`).
    #[serde(default)]
    pub max_bytes: Option<usize>,
    /// Optional long-poll wait in ms (clamped to 30000); blocks until input arrives or elapses. Omit/`0` returns immediately.
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

/// Response from `portal_projection_get_pending_input`.
#[derive(Debug, Serialize)]
pub struct PortalProjectionGetPendingInputResult {
    /// `true` when the authority accepted the poll.
    pub accepted: bool,
    /// HUD-originated input items delivered by this poll.
    pub items: Vec<crate::portal_op::PendingInputEntry>,
    /// Still-pending items that did not fit this response's item/byte budget.
    pub remaining_count: usize,
    /// Total byte size of still-pending items that did not fit.
    pub remaining_bytes: usize,
    /// Human-readable status summary.
    pub status_summary: String,
}

/// Drain HUD-originated pending input for a projection session (hud-bq0gl.1).
///
/// Forwards the poll through the portal-op channel to the winit event-loop
/// thread. The driver calls `handle_get_pending_input` on the authority, which
/// transitions matching items to `Delivered` and returns them. Delivered items
/// must subsequently be acknowledged via `portal_projection_acknowledge_input`.
///
/// # Errors
///
/// - `invalid_params` if `projection_id` or `owner_token` is empty.
/// - `internal` if the portal authority is not wired.
/// - `internal` if the authority rejected the poll (invalid / expired token).
pub async fn handle_portal_projection_get_pending_input(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
) -> McpResult<PortalProjectionGetPendingInputResult> {
    let render_wake = tze_hud_scene::render_wake::RenderWakeNotifier::default();
    handle_portal_projection_get_pending_input_with_render_wake(params, portal_op_tx, &render_wake)
        .await
}

pub(crate) async fn handle_portal_projection_get_pending_input_with_render_wake(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) -> McpResult<PortalProjectionGetPendingInputResult> {
    let p: PortalProjectionGetPendingInputParams = parse_params(params)?;

    if p.projection_id.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "projection_id must be non-empty".to_string(),
        ));
    }
    if p.owner_token.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "owner_token must be non-empty".to_string(),
        ));
    }

    let tx = portal_op_tx.ok_or_else(|| {
        McpError::Internal(
            "portal authority not wired — runtime must enable portal_op channel".to_string(),
        )
    })?;

    // Long-poll bound. The wait is served here on the async MCP side, NOT in the
    // windowed drain loop, so the runtime event loop is never blocked. Each
    // iteration is an ordinary poll; empty polls are side-effect-free (the
    // authority only transitions items to Delivered when it actually returns
    // them), so re-polling cannot drop or double-deliver input.
    const MAX_WAIT_MS: u64 = 30_000;
    const POLL_INTERVAL_MS: u64 = 150;
    let wait_ms = p.wait_ms.unwrap_or(0).min(MAX_WAIT_MS);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait_ms);

    loop {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(crate::portal_op::PortalOp::GetPendingInput {
            projection_id: p.projection_id.clone(),
            owner_token: p.owner_token.clone(),
            max_items: p.max_items,
            max_bytes: p.max_bytes,
            reply: reply_tx,
        })
        .map_err(|_| McpError::Internal("portal authority channel closed".to_string()))?;
        render_wake.notify();

        let batch = match reply_rx.await {
            Ok(Ok(batch)) => batch,
            Ok(Err(rejection)) => {
                return Err(McpError::ProjectionRejected {
                    error_code: rejection.error_code,
                    operation: "portal_projection_get_pending_input",
                });
            }
            Err(_) => {
                return Err(McpError::Internal(
                    "portal authority did not respond (channel dropped)".to_string(),
                ));
            }
        };

        let now = tokio::time::Instant::now();
        // Return as soon as there is *any* pending input to surface. A non-empty
        // batch is the obvious case; an empty batch with `remaining_count > 0` is
        // budget backpressure — input exists but did not fit this caller's
        // max_items/max_bytes budget. Re-polling cannot make an over-budget item
        // fit, so suppressing that signal until the wait expires would stall the
        // caller for up to 30s; surface it immediately so it can raise its cap
        // and re-call (hud-p4ufx review follow-up).
        if !batch.items.is_empty() || batch.remaining_count > 0 || now >= deadline {
            return Ok(PortalProjectionGetPendingInputResult {
                accepted: true,
                items: batch.items,
                remaining_count: batch.remaining_count,
                remaining_bytes: batch.remaining_bytes,
                status_summary: "pending input returned".to_string(),
            });
        }

        // No input yet and time remains: sleep a short interval, then re-poll.
        let remaining = deadline - now;
        tokio::time::sleep(remaining.min(std::time::Duration::from_millis(POLL_INTERVAL_MS))).await;
    }
}

// ─── portal_projection_acknowledge_input ─────────────────────────────────────

/// Parameters for `portal_projection_acknowledge_input`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PortalProjectionAcknowledgeInputParams {
    /// Projection session id from attach.
    pub projection_id: String,
    /// Owner token from attach.
    pub owner_token: String,
    /// Input item id from a `get_pending_input` response.
    pub input_id: String,
    /// `handled`|`deferred`|`rejected`.
    pub ack_state: String,
    /// Optional message recorded with the acknowledgement.
    #[serde(default)]
    pub ack_message: Option<String>,
    /// Optional re-delivery floor (wall-clock µs); valid only with `deferred`.
    #[serde(default)]
    pub not_before_wall_us: Option<u64>,
}

/// Response from `portal_projection_acknowledge_input`.
#[derive(Debug, Serialize)]
pub struct PortalProjectionAcknowledgeInputResult {
    /// `true` when the authority accepted the acknowledgement.
    pub accepted: bool,
    /// Human-readable status summary.
    pub status_summary: String,
}

/// Acknowledge a delivered input item for a projection session (hud-bq0gl.1).
///
/// Forwards the acknowledgement through the portal-op channel to the winit
/// event-loop thread. The driver calls `handle_acknowledge_input`, recording
/// the terminal (`handled` / `rejected`) or deferred disposition. Terminal
/// acknowledgement is idempotent; a conflicting terminal ack is rejected.
///
/// # Errors
///
/// - `invalid_params` if `projection_id`, `owner_token`, `input_id`, or
///   `ack_state` is empty.
/// - `internal` if the portal authority is not wired.
/// - `internal` if the authority rejected the acknowledgement (invalid token,
///   unrecognized `ack_state`, terminal conflict, etc.).
pub async fn handle_portal_projection_acknowledge_input(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
) -> McpResult<PortalProjectionAcknowledgeInputResult> {
    let render_wake = tze_hud_scene::render_wake::RenderWakeNotifier::default();
    handle_portal_projection_acknowledge_input_with_render_wake(params, portal_op_tx, &render_wake)
        .await
}

pub(crate) async fn handle_portal_projection_acknowledge_input_with_render_wake(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) -> McpResult<PortalProjectionAcknowledgeInputResult> {
    let p: PortalProjectionAcknowledgeInputParams = parse_params(params)?;

    if p.projection_id.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "projection_id must be non-empty".to_string(),
        ));
    }
    if p.owner_token.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "owner_token must be non-empty".to_string(),
        ));
    }
    if p.input_id.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "input_id must be non-empty".to_string(),
        ));
    }
    if p.ack_state.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "ack_state must be non-empty".to_string(),
        ));
    }

    let tx = portal_op_tx.ok_or_else(|| {
        McpError::Internal(
            "portal authority not wired — runtime must enable portal_op channel".to_string(),
        )
    })?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::portal_op::PortalOp::AcknowledgeInput {
        projection_id: p.projection_id,
        owner_token: p.owner_token,
        input_id: p.input_id,
        ack_state: p.ack_state,
        ack_message: p.ack_message,
        not_before_wall_us: p.not_before_wall_us,
        reply: reply_tx,
    })
    .map_err(|_| McpError::Internal("portal authority channel closed".to_string()))?;
    render_wake.notify();

    match reply_rx.await {
        Ok(Ok(())) => Ok(PortalProjectionAcknowledgeInputResult {
            accepted: true,
            status_summary: "input acknowledged".to_string(),
        }),
        Ok(Err(rejection)) => Err(McpError::ProjectionRejected {
            error_code: rejection.error_code,
            operation: "portal_projection_acknowledge_input",
        }),
        Err(_) => Err(McpError::Internal(
            "portal authority did not respond (channel dropped)".to_string(),
        )),
    }
}

// ─── portal_projection_detach ────────────────────────────────────────────────

/// Parameters for `portal_projection_detach`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PortalProjectionDetachParams {
    /// Projection session id from attach.
    pub projection_id: String,
    /// Owner token from attach.
    pub owner_token: String,
    /// Human-readable reason for the audit log.
    pub reason: String,
}

/// Response from `portal_projection_detach`.
#[derive(Debug, Serialize)]
pub struct PortalProjectionDetachResult {
    /// `true` when the authority accepted the detach.
    pub accepted: bool,
    /// Human-readable status summary.
    pub status_summary: String,
}

/// Detach a projection session, purging its private state (hud-bq0gl.1).
///
/// Forwards the detach through the portal-op channel to the winit event-loop
/// thread. The driver calls `handle_detach` (purging the session + coalescer
/// entry) and drops the drive entry / tile mapping. After detach the
/// `projection_id` is free to be re-attached.
///
/// # Errors
///
/// - `invalid_params` if `projection_id`, `owner_token`, or `reason` is empty.
/// - `internal` if the portal authority is not wired.
/// - `internal` if the authority rejected the detach (invalid / expired token).
pub async fn handle_portal_projection_detach(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
) -> McpResult<PortalProjectionDetachResult> {
    let render_wake = tze_hud_scene::render_wake::RenderWakeNotifier::default();
    handle_portal_projection_detach_with_render_wake(params, portal_op_tx, &render_wake).await
}

pub(crate) async fn handle_portal_projection_detach_with_render_wake(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) -> McpResult<PortalProjectionDetachResult> {
    let p: PortalProjectionDetachParams = parse_params(params)?;

    if p.projection_id.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "projection_id must be non-empty".to_string(),
        ));
    }
    if p.owner_token.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "owner_token must be non-empty".to_string(),
        ));
    }
    if p.reason.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "reason must be non-empty".to_string(),
        ));
    }

    let tx = portal_op_tx.ok_or_else(|| {
        McpError::Internal(
            "portal authority not wired — runtime must enable portal_op channel".to_string(),
        )
    })?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::portal_op::PortalOp::Detach {
        projection_id: p.projection_id,
        owner_token: p.owner_token,
        reason: p.reason,
        reply: reply_tx,
    })
    .map_err(|_| McpError::Internal("portal authority channel closed".to_string()))?;
    render_wake.notify();

    match reply_rx.await {
        Ok(Ok(())) => Ok(PortalProjectionDetachResult {
            accepted: true,
            status_summary: "projection detached and private state purged".to_string(),
        }),
        Ok(Err(rejection)) => Err(McpError::ProjectionRejected {
            error_code: rejection.error_code,
            operation: "portal_projection_detach",
        }),
        Err(_) => Err(McpError::Internal(
            "portal authority did not respond (channel dropped)".to_string(),
        )),
    }
}

// ─── portal_projection_cleanup ───────────────────────────────────────────────

/// Parameters for `portal_projection_cleanup`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PortalProjectionCleanupParams {
    /// Projection session id from attach.
    pub projection_id: String,
    /// `owner` or `operator`.
    pub cleanup_authority: String,
    /// Owner token (required when `cleanup_authority = owner`).
    #[serde(default)]
    pub owner_token: Option<String>,
    /// Operator credential (required when `cleanup_authority = operator`).
    #[serde(default)]
    pub operator_authority: Option<String>,
    /// Human-readable reason for the audit log.
    pub reason: String,
}

/// Response from `portal_projection_cleanup`.
#[derive(Debug, Serialize)]
pub struct PortalProjectionCleanupResult {
    /// `true` when the authority accepted the cleanup.
    pub accepted: bool,
    /// Human-readable status summary.
    pub status_summary: String,
}

/// Cleanup a projection session, purging its private state (hud-vkisv).
///
/// This is deliberately distinct from owner `detach`: owner cleanup requires the
/// owner token, while operator cleanup requires a separate operator-authority
/// credential and forwards no owner token.
///
/// # Errors
///
/// - `invalid_params` if `projection_id`, `cleanup_authority`, the selected
///   credential field, or `reason` is empty.
/// - `invalid_params` if `cleanup_authority` is not `owner` or `operator`.
/// - `internal` if the portal authority is not wired.
/// - `internal` if the authority rejected cleanup (invalid credential, missing
///   configured operator authority, projection not found, etc.).
pub async fn handle_portal_projection_cleanup(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
) -> McpResult<PortalProjectionCleanupResult> {
    let render_wake = tze_hud_scene::render_wake::RenderWakeNotifier::default();
    handle_portal_projection_cleanup_with_render_wake(params, portal_op_tx, &render_wake).await
}

pub(crate) async fn handle_portal_projection_cleanup_with_render_wake(
    params: Value,
    portal_op_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) -> McpResult<PortalProjectionCleanupResult> {
    let p: PortalProjectionCleanupParams = parse_params(params)?;

    if p.projection_id.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "projection_id must be non-empty".to_string(),
        ));
    }
    let cleanup_authority = p.cleanup_authority.trim();
    if cleanup_authority.is_empty() {
        return Err(McpError::InvalidParams(
            "cleanup_authority must be non-empty".to_string(),
        ));
    }
    if p.reason.trim().is_empty() {
        return Err(McpError::InvalidParams(
            "reason must be non-empty".to_string(),
        ));
    }

    let (owner_token, operator_authority) = match cleanup_authority {
        "owner" => {
            let token = p.owner_token.ok_or_else(|| {
                McpError::InvalidParams("owner_token must be non-empty".to_string())
            })?;
            if token.trim().is_empty() {
                return Err(McpError::InvalidParams(
                    "owner_token must be non-empty".to_string(),
                ));
            }
            (Some(token), None)
        }
        "operator" => {
            let credential = p.operator_authority.ok_or_else(|| {
                McpError::InvalidParams("operator_authority must be non-empty".to_string())
            })?;
            if credential.trim().is_empty() {
                return Err(McpError::InvalidParams(
                    "operator_authority must be non-empty".to_string(),
                ));
            }
            (None, Some(credential))
        }
        other => {
            return Err(McpError::InvalidParams(format!(
                "invalid cleanup_authority {other:?}: expected owner or operator"
            )));
        }
    };

    let tx = portal_op_tx.ok_or_else(|| {
        McpError::Internal(
            "portal authority not wired — runtime must enable portal_op channel".to_string(),
        )
    })?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::portal_op::PortalOp::Cleanup {
        projection_id: p.projection_id,
        cleanup_authority: cleanup_authority.to_string(),
        owner_token,
        operator_authority,
        reason: p.reason,
        reply: reply_tx,
    })
    .map_err(|_| McpError::Internal("portal authority channel closed".to_string()))?;
    render_wake.notify();

    match reply_rx.await {
        Ok(Ok(())) => Ok(PortalProjectionCleanupResult {
            accepted: true,
            status_summary: "projection cleanup accepted and private state purged".to_string(),
        }),
        Ok(Err(rejection)) => Err(McpError::ProjectionRejected {
            error_code: rejection.error_code,
            operation: "portal_projection_cleanup",
        }),
        Err(_) => Err(McpError::Internal(
            "portal authority did not respond (channel dropped)".to_string(),
        )),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tze_hud_scene::{
        SceneId,
        graph::SceneGraph,
        types::{
            ContentionPolicy, GeometryPolicy, LayerAttachment, RenderingPolicy, ZoneDefinition,
            ZoneMediaType,
        },
    };

    fn scene_with_tab() -> (SceneGraph, SceneId) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).expect("create tab");
        (scene, tab_id)
    }

    // ── create_tab ──────────────────────────────────────────────────────────

    #[test]
    fn test_create_tab_basic() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let result = handle_create_tab(json!({"name": "Dashboard"}), &mut scene).unwrap();
        assert_eq!(result.name, "Dashboard");
        assert_eq!(result.display_order, 0);
        assert_eq!(scene.tabs.len(), 1);
    }

    #[test]
    fn test_create_tab_explicit_order() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let result =
            handle_create_tab(json!({"name": "Tab", "display_order": 5}), &mut scene).unwrap();
        assert_eq!(result.display_order, 5);
    }

    #[test]
    fn test_create_tab_auto_increments_order() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        handle_create_tab(json!({"name": "A", "display_order": 3}), &mut scene).unwrap();
        let r = handle_create_tab(json!({"name": "B"}), &mut scene).unwrap();
        assert_eq!(r.display_order, 4);
    }

    #[test]
    fn test_create_tab_empty_name_fails() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let err = handle_create_tab(json!({"name": ""}), &mut scene).unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    #[test]
    fn test_create_tab_duplicate_order_fails() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        handle_create_tab(json!({"name": "A", "display_order": 0}), &mut scene).unwrap();
        let err =
            handle_create_tab(json!({"name": "B", "display_order": 0}), &mut scene).unwrap_err();
        assert!(matches!(err, McpError::SceneError(_)));
    }

    // ── create_tile ─────────────────────────────────────────────────────────

    #[test]
    fn test_create_tile_basic() {
        let (mut scene, _tab_id) = scene_with_tab();
        let result = handle_create_tile(
            json!({
                "namespace": "agent-1",
                "bounds": {"x": 0.0, "y": 0.0, "width": 400.0, "height": 300.0}
            }),
            &mut scene,
        )
        .unwrap();
        assert!(!result.tile_id.is_empty());
        assert_eq!(result.namespace, "agent-1");
        assert_eq!(scene.tile_count(), 1);
    }

    #[test]
    fn test_create_tile_explicit_tab() {
        let (mut scene, tab_id) = scene_with_tab();
        let result = handle_create_tile(
            json!({
                "tab_id": tab_id.to_string(),
                "namespace": "agent-1",
                "bounds": {"x": 0.0, "y": 0.0, "width": 200.0, "height": 200.0}
            }),
            &mut scene,
        )
        .unwrap();
        assert_eq!(result.tab_id, tab_id.to_string());
    }

    #[test]
    fn test_create_tile_no_active_tab_fails() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let err = handle_create_tile(
            json!({
                "namespace": "agent-1",
                "bounds": {"x": 0.0, "y": 0.0, "width": 200.0, "height": 200.0}
            }),
            &mut scene,
        )
        .unwrap_err();
        assert!(matches!(err, McpError::NoActiveTab));
    }

    #[test]
    fn test_create_tile_invalid_bounds_fails() {
        let (mut scene, _) = scene_with_tab();
        let err = handle_create_tile(
            json!({
                "namespace": "agent-1",
                "bounds": {"x": 0.0, "y": 0.0, "width": 0.0, "height": 300.0}
            }),
            &mut scene,
        )
        .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    #[test]
    fn test_create_tile_empty_namespace_fails() {
        let (mut scene, _) = scene_with_tab();
        let err = handle_create_tile(
            json!({
                "namespace": "",
                "bounds": {"x": 0.0, "y": 0.0, "width": 200.0, "height": 200.0}
            }),
            &mut scene,
        )
        .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    #[test]
    fn test_create_tile_grants_lease() {
        let (mut scene, _) = scene_with_tab();
        handle_create_tile(
            json!({
                "namespace": "agent-1",
                "bounds": {"x": 0.0, "y": 0.0, "width": 200.0, "height": 200.0}
            }),
            &mut scene,
        )
        .unwrap();
        assert_eq!(scene.leases.len(), 1);
    }

    // ── set_content ─────────────────────────────────────────────────────────

    #[test]
    fn test_set_content_basic() {
        let (mut scene, _) = scene_with_tab();
        let tile = handle_create_tile(
            json!({
                "namespace": "agent-1",
                "bounds": {"x": 0.0, "y": 0.0, "width": 400.0, "height": 300.0}
            }),
            &mut scene,
        )
        .unwrap();

        let result = handle_set_content(
            json!({"tile_id": tile.tile_id, "content": "# Hello"}),
            &mut scene,
        )
        .unwrap();

        assert_eq!(result.tile_id, tile.tile_id);
        assert_eq!(result.content_len, 7);
        assert_eq!(scene.node_count(), 1);
    }

    #[test]
    fn test_set_content_replaces_existing() {
        let (mut scene, _) = scene_with_tab();
        let tile = handle_create_tile(
            json!({
                "namespace": "a",
                "bounds": {"x": 0.0, "y": 0.0, "width": 400.0, "height": 300.0}
            }),
            &mut scene,
        )
        .unwrap();

        handle_set_content(
            json!({"tile_id": tile.tile_id, "content": "First"}),
            &mut scene,
        )
        .unwrap();
        assert_eq!(scene.node_count(), 1);

        handle_set_content(
            json!({"tile_id": tile.tile_id, "content": "Second"}),
            &mut scene,
        )
        .unwrap();
        // Root replaced; still exactly 1 node
        assert_eq!(scene.node_count(), 1);
    }

    #[test]
    fn test_set_content_empty_content_fails() {
        let (mut scene, _) = scene_with_tab();
        let tile = handle_create_tile(
            json!({
                "namespace": "a",
                "bounds": {"x": 0.0, "y": 0.0, "width": 200.0, "height": 200.0}
            }),
            &mut scene,
        )
        .unwrap();
        let err = handle_set_content(json!({"tile_id": tile.tile_id, "content": ""}), &mut scene)
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    #[test]
    fn test_set_content_invalid_tile_id_fails() {
        let (mut scene, _) = scene_with_tab();
        let err = handle_set_content(
            json!({"tile_id": "not-a-uuid", "content": "hello"}),
            &mut scene,
        )
        .unwrap_err();
        assert!(matches!(err, McpError::InvalidId(_)));
    }

    #[test]
    fn test_set_content_nonexistent_tile_fails() {
        let (mut scene, _) = scene_with_tab();
        let fake_id = SceneId::new().to_string();
        let err = handle_set_content(json!({"tile_id": fake_id, "content": "hello"}), &mut scene)
            .unwrap_err();
        assert!(matches!(err, McpError::SceneError(_)));
    }

    // ── publish_to_zone ─────────────────────────────────────────────────────

    fn scene_with_zone() -> (SceneGraph, SceneId, String) {
        let (mut scene, tab_id) = scene_with_tab();
        let zone_name = "main-overlay".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "Primary overlay zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.0,
                    width_pct: 1.0,
                    height_pct: 0.1,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 4,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );
        (scene, tab_id, zone_name)
    }

    #[test]
    fn test_publish_to_zone_basic() {
        let (mut scene, _, zone) = scene_with_zone();
        let result = handle_publish_to_zone(
            json!({"zone_name": zone, "content": "## Status: OK"}),
            &mut scene,
        )
        .unwrap();
        assert_eq!(result.zone_name, zone);
        // Publishing goes to zone_registry.active_publishes; tiles are compositor-resolved.
        assert_eq!(scene.tile_count(), 0);
        assert_eq!(scene.node_count(), 0);
        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(publishes.len(), 1);
        assert!(
            matches!(&publishes[0].content, tze_hud_scene::types::ZoneContent::StreamText(s) if s == "## Status: OK")
        );
    }

    #[test]
    fn test_publish_to_zone_with_ttl_us() {
        let (mut scene, _, zone) = scene_with_zone();
        let result = handle_publish_to_zone(
            json!({"zone_name": zone, "content": "hello", "ttl_us": 120_000_000u64}),
            &mut scene,
        )
        .unwrap();
        assert_eq!(result.ttl_us, 120_000_000u64);
    }

    /// Regression (hud-vfwb1): a zone publish carrying an explicit `ttl_us` MUST
    /// set a content-level expiry on the `ZonePublishRecord`, so the per-frame
    /// sweep (`drain_expired_zone_publications`) removes the content once the
    /// deadline passes.
    ///
    /// Before the fix, `handle_publish_to_zone` applied `ttl_us` only to the
    /// lease grant and left `ZonePublishRecord.expires_at_wall_us = None`, so a
    /// subtitle published with `ttl_us` stayed painted indefinitely — nothing
    /// ever marked it expired, and with no subsequent publish it was never swept
    /// (the idle present-gate had nothing dirty to react to). This test drives a
    /// deterministic TestClock past the TTL and asserts the record both carries
    /// the expiry and is swept, bumping `scene.version` to re-arm the gate.
    #[test]
    fn test_publish_to_zone_ttl_sets_content_expiry_and_is_swept() {
        use std::sync::Arc;

        // TestClock: value is milliseconds; now_us() = ms * 1000. Start at 1s.
        let clock = tze_hud_scene::TestClock::new(1_000);
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let zone = "subtitle".to_string();
        scene.zone_registry.zones.insert(
            zone.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone.clone(),
                description: "Subtitle overlay zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.9,
                    width_pct: 1.0,
                    height_pct: 0.1,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 4,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );

        // Live repro: subtitle published with ttl_us = 20_000_000 (20 s).
        handle_publish_to_zone(
            json!({"zone_name": zone, "content": "expiring subtitle", "ttl_us": 20_000_000u64}),
            &mut scene,
        )
        .unwrap();

        // The publish record MUST carry an absolute content-level expiry derived
        // from ttl_us: now_us (1_000_000) + ttl_us (20_000_000) = 21_000_000.
        let rec = &scene.zone_registry.active_publishes.get(&zone).unwrap()[0];
        assert_eq!(
            rec.expires_at_wall_us,
            Some(21_000_000),
            "ttl_us must be converted into an absolute content expiry on the record"
        );

        // Before the deadline (advance to 20 s < 21 s): sweep keeps it.
        clock.advance(19_000);
        assert_eq!(
            scene.drain_expired_zone_publications(),
            0,
            "publication must survive before its TTL deadline"
        );
        assert_eq!(
            scene
                .zone_registry
                .active_publishes
                .get(&zone)
                .unwrap()
                .len(),
            1,
            "subtitle must still be present 1 s before its TTL"
        );

        // Past the deadline (advance to 22 s > 21 s): sweep removes it and bumps
        // scene.version so the idle present-gate re-arms and repaints.
        let version_before = scene.version;
        clock.advance(2_000);
        assert_eq!(
            scene.drain_expired_zone_publications(),
            1,
            "expired subtitle must be swept once its TTL deadline passes"
        );
        let after = scene.zone_registry.active_publishes.get(&zone);
        assert!(
            after.is_none() || after.unwrap().is_empty(),
            "zone must hold no active publications after TTL expiry"
        );
        assert!(
            scene.version > version_before,
            "expiry sweep must bump scene.version to re-arm the idle present-gate"
        );
    }

    #[test]
    fn test_publish_to_zone_with_merge_key() {
        let (mut scene, _, zone) = scene_with_zone();
        let result = handle_publish_to_zone(
            json!({"zone_name": zone, "content": "hello", "merge_key": "subtitle-main"}),
            &mut scene,
        )
        .unwrap();
        assert_eq!(result.merge_key.as_deref(), Some("subtitle-main"));
    }

    #[test]
    fn test_publish_to_zone_unknown_zone_fails() {
        let (mut scene, _, _) = scene_with_zone();
        let err = handle_publish_to_zone(
            json!({"zone_name": "does-not-exist", "content": "hi"}),
            &mut scene,
        )
        .unwrap_err();
        assert!(matches!(err, McpError::ZoneNotFound(_)));
    }

    #[test]
    fn test_publish_to_zone_no_tab_succeeds() {
        // Zone publishing is global (not tab-scoped) in v1. No active tab is required.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let zone_name = "z".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "z".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.0,
                    width_pct: 1.0,
                    height_pct: 0.1,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 4,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );
        let result =
            handle_publish_to_zone(json!({"zone_name": zone_name, "content": "hi"}), &mut scene)
                .unwrap();
        assert_eq!(result.zone_name, zone_name);
        assert!(
            scene
                .zone_registry
                .active_publishes
                .contains_key(&zone_name)
        );
    }

    #[test]
    fn test_publish_to_zone_contention_policy_latest_wins() {
        // scene_with_zone creates a LatestWins zone; a second publish must replace
        // the first (single record in active_publishes after both calls).
        let (mut scene, _, zone) = scene_with_zone();
        handle_publish_to_zone(json!({"zone_name": zone, "content": "first"}), &mut scene).unwrap();
        handle_publish_to_zone(json!({"zone_name": zone, "content": "second"}), &mut scene)
            .unwrap();
        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(publishes.len(), 1, "LatestWins must replace old record");
        assert!(
            matches!(&publishes[0].content, tze_hud_scene::types::ZoneContent::StreamText(s) if s == "second"),
            "latest content must win"
        );
    }

    #[test]
    fn test_publish_to_zone_empty_content_fails() {
        let (mut scene, _, zone) = scene_with_zone();
        let err = handle_publish_to_zone(json!({"zone_name": zone, "content": ""}), &mut scene)
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    // ── list_zones ──────────────────────────────────────────────────────────

    #[test]
    fn test_list_zones_empty() {
        let scene = SceneGraph::new(1920.0, 1080.0);
        let result = handle_list_zones(json!(null), &scene).unwrap();
        assert_eq!(result.count, 0);
        assert!(result.zones.is_empty());
    }

    #[test]
    fn test_list_zones_returns_registered() {
        let (scene, _, zone) = scene_with_zone();
        let result = handle_list_zones(json!(null), &scene).unwrap();
        assert_eq!(result.count, 1);
        assert_eq!(result.zones[0].name, zone);
    }

    #[test]
    fn test_list_zones_has_content_flag() {
        let (mut scene, _, zone) = scene_with_zone();
        // Before publishing: zone_registry.active_publishes is empty → no content
        let before = handle_list_zones(json!(null), &scene).unwrap();
        assert!(!before.zones[0].has_content);

        // After publishing: active_publishes contains a record → has_content = true
        // (namespace argument is used for the lease; the zone name drives the publish)
        handle_publish_to_zone(
            json!({"zone_name": zone.clone(), "content": "hi", "namespace": zone.clone()}),
            &mut scene,
        )
        .unwrap();
        let after = handle_list_zones(json!(null), &scene).unwrap();
        assert!(after.zones[0].has_content);
    }

    #[test]
    fn test_list_zones_sorted_by_name() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        scene.create_tab("Main", 0).unwrap();
        for name in ["zebra", "alpha", "mango"] {
            scene.zone_registry.zones.insert(
                name.to_string(),
                ZoneDefinition {
                    id: SceneId::new(),
                    name: name.to_string(),
                    description: "".to_string(),
                    geometry_policy: GeometryPolicy::Relative {
                        x_pct: 0.0,
                        y_pct: 0.0,
                        width_pct: 1.0,
                        height_pct: 0.1,
                    },
                    accepted_media_types: vec![ZoneMediaType::StreamText],
                    rendering_policy: RenderingPolicy::default(),
                    contention_policy: ContentionPolicy::LatestWins,
                    max_publishers: 4,
                    transport_constraint: None,
                    auto_clear_ms: None,
                    ephemeral: false,
                    layer_attachment: LayerAttachment::Content,
                },
            );
        }
        let result = handle_list_zones(json!(null), &scene).unwrap();
        let names: Vec<&str> = result.zones.iter().map(|z| z.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mango", "zebra"]);
    }

    #[test]
    fn test_list_zones_invalid_params_fails() {
        let scene = SceneGraph::new(1920.0, 1080.0);
        let err = handle_list_zones(json!("unexpected-string"), &scene).unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    // ── Contention policy: Stack ─────────────────────────────────────────────

    /// Build a scene with a Stack zone (max_depth=3).
    fn scene_with_stack_zone() -> (SceneGraph, String) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let zone_name = "notif".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "Stack zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.75,
                    y_pct: 0.0,
                    width_pct: 0.25,
                    height_pct: 0.30,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::Stack { max_depth: 3 },
                max_publishers: 8,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );
        (scene, zone_name)
    }

    #[test]
    fn test_contention_stack_accumulates_records() {
        let (mut scene, zone) = scene_with_stack_zone();
        // Three publishes — all should accumulate in the stack.
        for i in 1..=3u32 {
            handle_publish_to_zone(
                json!({"zone_name": zone, "content": format!("msg-{i}"), "namespace": format!("agent-{i}")}),
                &mut scene,
            )
            .unwrap();
        }
        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            publishes.len(),
            3,
            "Stack zone must accumulate all records up to max_depth"
        );
    }

    #[test]
    fn test_contention_stack_trims_oldest_when_max_depth_exceeded() {
        let (mut scene, zone) = scene_with_stack_zone();
        // Publish 4 items to a max_depth=3 stack (different namespaces to avoid publisher limit).
        for i in 1..=4u32 {
            handle_publish_to_zone(
                json!({"zone_name": zone, "content": format!("msg-{i}"), "namespace": format!("agent-{i}")}),
                &mut scene,
            )
            .unwrap();
        }
        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            publishes.len(),
            3,
            "Stack must trim oldest when max_depth exceeded"
        );
        // Oldest (msg-1) should be gone; most recent (msg-4) should be present.
        assert!(
            publishes
                .iter()
                .all(|r| r.content
                    != tze_hud_scene::types::ZoneContent::StreamText("msg-1".to_string())),
            "oldest record must be evicted when stack overflows"
        );
        assert!(
            publishes
                .iter()
                .any(|r| r.content
                    == tze_hud_scene::types::ZoneContent::StreamText("msg-4".to_string())),
            "newest record must survive stack trim"
        );
    }

    #[test]
    fn test_contention_stack_no_tiles_created() {
        // Zone publishes must never create tiles directly — compositor handles rendering.
        let (mut scene, zone) = scene_with_stack_zone();
        for i in 1..=3u32 {
            handle_publish_to_zone(
                json!({"zone_name": zone, "content": format!("item-{i}"), "namespace": format!("ns-{i}")}),
                &mut scene,
            )
            .unwrap();
        }
        assert_eq!(
            scene.tile_count(),
            0,
            "Stack zone publishes must not create tiles"
        );
        assert_eq!(
            scene.node_count(),
            0,
            "Stack zone publishes must not create nodes"
        );
    }

    // ── Contention policy: Replace ───────────────────────────────────────────

    /// Build a scene with a Replace zone (single occupant).
    fn scene_with_replace_zone() -> (SceneGraph, String) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let zone_name = "pip".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "Replace zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.0,
                    width_pct: 0.5,
                    height_pct: 0.5,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::Replace,
                max_publishers: 1,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );
        (scene, zone_name)
    }

    #[test]
    fn test_contention_replace_evicts_current_occupant() {
        let (mut scene, zone) = scene_with_replace_zone();
        handle_publish_to_zone(
            json!({"zone_name": zone, "content": "first-occupant", "namespace": "agent-a"}),
            &mut scene,
        )
        .unwrap();
        handle_publish_to_zone(
            json!({"zone_name": zone, "content": "second-occupant", "namespace": "agent-b"}),
            &mut scene,
        )
        .unwrap();
        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            publishes.len(),
            1,
            "Replace zone must hold exactly one record"
        );
        assert!(
            matches!(&publishes[0].content, tze_hud_scene::types::ZoneContent::StreamText(s) if s == "second-occupant"),
            "Replace must evict first and install second occupant"
        );
    }

    #[test]
    fn test_contention_replace_no_tiles_created() {
        let (mut scene, zone) = scene_with_replace_zone();
        handle_publish_to_zone(
            json!({"zone_name": zone, "content": "occupant", "namespace": "agent-x"}),
            &mut scene,
        )
        .unwrap();
        assert_eq!(
            scene.tile_count(),
            0,
            "Replace zone publishes must not create tiles"
        );
    }

    // ── Contention policy: MergeByKey ────────────────────────────────────────

    /// Build a scene with a MergeByKey zone (max_keys=4).
    fn scene_with_merge_zone() -> (SceneGraph, String) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let zone_name = "status".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "MergeByKey zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.95,
                    width_pct: 1.0,
                    height_pct: 0.05,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::MergeByKey { max_keys: 4 },
                max_publishers: 16,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );
        (scene, zone_name)
    }

    #[test]
    fn test_contention_merge_by_key_same_key_replaces() {
        let (mut scene, zone) = scene_with_merge_zone();
        // Publish twice with the same merge_key — must stay at 1 record.
        handle_publish_to_zone(
            json!({"zone_name": zone, "content": "v1", "merge_key": "cpu", "namespace": "agent-a"}),
            &mut scene,
        )
        .unwrap();
        handle_publish_to_zone(
            json!({"zone_name": zone, "content": "v2", "merge_key": "cpu", "namespace": "agent-a"}),
            &mut scene,
        )
        .unwrap();
        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(publishes.len(), 1, "Same merge_key must replace old record");
        assert!(
            matches!(&publishes[0].content, tze_hud_scene::types::ZoneContent::StreamText(s) if s == "v2"),
            "latest content must win for same merge_key"
        );
    }

    #[test]
    fn test_contention_merge_by_key_different_keys_coexist() {
        let (mut scene, zone) = scene_with_merge_zone();
        handle_publish_to_zone(
            json!({"zone_name": zone, "content": "cpu-data", "merge_key": "cpu", "namespace": "agent-a"}),
            &mut scene,
        )
        .unwrap();
        handle_publish_to_zone(
            json!({"zone_name": zone, "content": "mem-data", "merge_key": "mem", "namespace": "agent-b"}),
            &mut scene,
        )
        .unwrap();
        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            publishes.len(),
            2,
            "Different merge_keys must coexist in zone"
        );
    }

    #[test]
    fn test_contention_merge_by_key_max_keys_evicts_oldest() {
        // When a MergeByKey zone is at max_keys capacity, publishing a new
        // distinct key must succeed by evicting the oldest entry (index 0).
        // Spec: openspec/changes/exemplar-status-bar/tasks.md §2.5
        let (mut scene, zone) = scene_with_merge_zone(); // max_keys = 4
        // Fill all 4 key slots; key-0 is inserted first (oldest).
        for i in 0..4u32 {
            handle_publish_to_zone(
                json!({"zone_name": zone, "content": format!("val-{i}"), "merge_key": format!("key-{i}"), "namespace": format!("agent-{i}")}),
                &mut scene,
            )
            .unwrap();
        }
        // 5th distinct key must SUCCEED — evicting the oldest entry.
        handle_publish_to_zone(
            json!({"zone_name": zone, "content": "overflow", "merge_key": "key-overflow", "namespace": "agent-x"}),
            &mut scene,
        )
        .expect("5th key must succeed: oldest evicted, max_keys remain");

        // Zone must retain exactly max_keys (4) publications.
        let pubs = scene.zone_registry.active_for_zone(&zone);
        assert_eq!(
            pubs.len(),
            4,
            "zone must retain exactly 4 publications after eviction"
        );
        // The oldest key ("key-0") must have been evicted.
        assert!(
            !pubs.iter().any(|r| r.merge_key.as_deref() == Some("key-0")),
            "key-0 (oldest) must have been evicted"
        );
        // The new key must be present.
        assert!(
            pubs.iter()
                .any(|r| r.merge_key.as_deref() == Some("key-overflow")),
            "key-overflow must be present after eviction"
        );
    }

    #[test]
    fn test_contention_merge_by_key_no_tiles_created() {
        let (mut scene, zone) = scene_with_merge_zone();
        handle_publish_to_zone(
            json!({"zone_name": zone, "content": "data", "merge_key": "k1", "namespace": "agent-a"}),
            &mut scene,
        )
        .unwrap();
        assert_eq!(
            scene.tile_count(),
            0,
            "MergeByKey zone publishes must not create tiles"
        );
    }

    // ── Media-type rejection ─────────────────────────────────────────────────

    #[test]
    fn test_media_type_rejected_for_wrong_type_zone() {
        // Build a zone that only accepts ShortTextWithIcon (not StreamText).
        // A plain string content is parsed as StreamText, so this must be rejected.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let zone_name = "notif-only".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "Notification-only zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.75,
                    y_pct: 0.0,
                    width_pct: 0.25,
                    height_pct: 0.20,
                },
                accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::Stack { max_depth: 8 },
                max_publishers: 16,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );
        // A plain string produces ZoneContent::StreamText.
        // ShortTextWithIcon zone must reject it.
        let err = handle_publish_to_zone(
            json!({"zone_name": zone_name, "content": "hello"}),
            &mut scene,
        )
        .unwrap_err();
        assert!(
            matches!(err, McpError::SceneError(_)),
            "StreamText publish to ShortTextWithIcon-only zone must return SceneError (media type mismatch), got: {err:?}"
        );
    }

    #[test]
    fn test_media_type_accepted_for_matching_zone() {
        // StreamText zone must accept StreamText content.
        let (mut scene, _, zone) = scene_with_zone();
        let result = handle_publish_to_zone(
            json!({"zone_name": zone, "content": "valid stream text"}),
            &mut scene,
        );
        assert!(
            result.is_ok(),
            "StreamText content must be accepted by StreamText zone"
        );
    }

    // ── Occupancy reporting (list_zones has_content accuracy) ────────────────

    #[test]
    fn test_has_content_false_before_publish() {
        let (scene, _, zone) = scene_with_zone();
        let result = handle_list_zones(json!(null), &scene).unwrap();
        let entry = result.zones.iter().find(|z| z.name == zone).unwrap();
        assert!(
            !entry.has_content,
            "has_content must be false before any publish"
        );
    }

    #[test]
    fn test_has_content_true_after_publish() {
        let (mut scene, _, zone) = scene_with_zone();
        handle_publish_to_zone(
            json!({"zone_name": zone.clone(), "content": "occupying content"}),
            &mut scene,
        )
        .unwrap();
        let result = handle_list_zones(json!(null), &scene).unwrap();
        let entry = result.zones.iter().find(|z| z.name == zone).unwrap();
        assert!(
            entry.has_content,
            "has_content must be true after successful publish"
        );
    }

    #[test]
    fn test_has_content_after_replace_policy_publish() {
        let (mut scene, zone) = scene_with_replace_zone();
        // Two publishes with Replace policy — one record remains; has_content = true.
        handle_publish_to_zone(
            json!({"zone_name": zone.clone(), "content": "first", "namespace": "a"}),
            &mut scene,
        )
        .unwrap();
        handle_publish_to_zone(
            json!({"zone_name": zone.clone(), "content": "second", "namespace": "b"}),
            &mut scene,
        )
        .unwrap();
        let result = handle_list_zones(json!(null), &scene).unwrap();
        let entry = result.zones.iter().find(|z| z.name == zone).unwrap();
        assert!(
            entry.has_content,
            "has_content must be true after Replace publish"
        );
        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            publishes.len(),
            1,
            "Replace must maintain exactly one record"
        );
    }

    #[test]
    fn test_has_content_false_after_zone_cleared() {
        let (mut scene, _, zone) = scene_with_zone();
        handle_publish_to_zone(
            json!({"zone_name": zone.clone(), "content": "something"}),
            &mut scene,
        )
        .unwrap();
        // Manually clear zone (as runtime would do on lease expiry / eviction).
        scene.clear_zone(&zone).unwrap();
        let result = handle_list_zones(json!(null), &scene).unwrap();
        let entry = result.zones.iter().find(|z| z.name == zone).unwrap();
        assert!(
            !entry.has_content,
            "has_content must be false after zone is cleared"
        );
    }

    #[test]
    fn test_has_content_stack_zone_reflects_occupancy() {
        let (mut scene, zone) = scene_with_stack_zone();
        // Stack with 2 items — has_content = true.
        for i in 1..=2u32 {
            handle_publish_to_zone(
                json!({"zone_name": zone.clone(), "content": format!("item-{i}"), "namespace": format!("agent-{i}")}),
                &mut scene,
            )
            .unwrap();
        }
        let result = handle_list_zones(json!(null), &scene).unwrap();
        let entry = result.zones.iter().find(|z| z.name == zone).unwrap();
        assert!(
            entry.has_content,
            "has_content must be true when Stack zone has entries"
        );
    }

    #[test]
    fn test_has_content_merge_zone_with_multiple_keys() {
        let (mut scene, zone) = scene_with_merge_zone();
        handle_publish_to_zone(
            json!({"zone_name": zone.clone(), "content": "data-a", "merge_key": "alpha", "namespace": "agent-a"}),
            &mut scene,
        )
        .unwrap();
        handle_publish_to_zone(
            json!({"zone_name": zone.clone(), "content": "data-b", "merge_key": "beta", "namespace": "agent-b"}),
            &mut scene,
        )
        .unwrap();
        let result = handle_list_zones(json!(null), &scene).unwrap();
        let entry = result.zones.iter().find(|z| z.name == zone).unwrap();
        assert!(
            entry.has_content,
            "has_content must be true when MergeByKey zone has keyed records"
        );
    }

    // ── Expiry/cleanup after lease loss ─────────────────────────────────────

    #[test]
    fn test_zone_publishes_cleared_on_lease_revoke() {
        // Publish to a zone, then explicitly revoke the lease and verify that
        // active_publishes for this namespace are cleaned up.
        let (mut scene, _, zone) = scene_with_zone();

        // publish_to_zone grants a lease internally; but we need the lease_id to revoke.
        // Grant a lease manually, then use publish_to_zone (bypassing the MCP handler
        // since we need direct SceneGraph access to revoke the lease by ID).
        use tze_hud_scene::types::{Capability, ZoneContent};
        let ns = "agent-expiry";
        let lease_id = scene.grant_lease(ns, 60_000, vec![Capability::PublishZone(zone.clone())]);
        scene
            .publish_to_zone(
                &zone,
                ZoneContent::StreamText("expiring content".to_string()),
                ns,
                None,
                None,
                None,
            )
            .unwrap();

        // Confirm content is present
        assert!(
            scene
                .zone_registry
                .active_publishes
                .get(&zone)
                .is_some_and(|v| !v.is_empty()),
            "zone must have content before lease revoke"
        );

        // Revoke the lease — spec §Requirement: Lease Revocation Clears Zone Publications
        scene.revoke_lease(lease_id).unwrap();

        // All publications for this namespace must be gone.
        let remaining = scene.zone_registry.active_publishes.get(&zone);
        assert!(
            remaining.is_none_or(|v| v.is_empty()),
            "zone publications must be cleared when lease is revoked"
        );
    }

    #[test]
    fn test_list_zones_has_content_false_after_lease_revoke() {
        // After lease revoke, list_zones must report has_content = false.
        let (mut scene, _, zone) = scene_with_zone();
        use tze_hud_scene::types::{Capability, ZoneContent};
        let ns = "agent-expiry-2";
        let lease_id = scene.grant_lease(ns, 60_000, vec![Capability::PublishZone(zone.clone())]);
        scene
            .publish_to_zone(
                &zone,
                ZoneContent::StreamText("content".to_string()),
                ns,
                None,
                None,
                None,
            )
            .unwrap();
        scene.revoke_lease(lease_id).unwrap();

        let result = handle_list_zones(json!(null), &scene).unwrap();
        let entry = result.zones.iter().find(|z| z.name == zone).unwrap();
        assert!(
            !entry.has_content,
            "list_zones has_content must be false after lease revoke clears zone publications"
        );
    }

    #[test]
    fn test_zone_publish_fails_without_active_lease() {
        // After lease revoke, publish_to_zone_with_lease must fail.
        let (mut scene, _, zone) = scene_with_zone();
        use tze_hud_scene::types::{Capability, ZoneContent};
        let ns = "agent-gone";
        let lease_id = scene.grant_lease(ns, 60_000, vec![Capability::PublishZone(zone.clone())]);
        scene.revoke_lease(lease_id).unwrap();

        // Now publish_to_zone_with_lease must reject (no active lease).
        let result = scene.publish_to_zone_with_lease(
            &zone,
            ZoneContent::StreamText("should fail".to_string()),
            ns,
            None,
            None,
        );
        assert!(
            result.is_err(),
            "publish must be rejected after lease revoke"
        );
    }

    // ── Guest vs resident capability gates (additional coverage) ────────────

    #[test]
    fn test_publish_to_zone_is_guest_accessible() {
        // publish_to_zone is a guest tool; callers without resident_mcp must succeed.
        let (mut scene, _, zone) = scene_with_zone();
        let result = handle_publish_to_zone(
            json!({"zone_name": zone, "content": "guest-publish"}),
            &mut scene,
        );
        assert!(
            result.is_ok(),
            "publish_to_zone must be callable without resident_mcp capability"
        );
    }

    #[test]
    fn test_list_zones_is_guest_accessible() {
        // list_zones is a guest tool.
        let (scene, _, _) = scene_with_zone();
        let result = handle_list_zones(json!(null), &scene);
        assert!(
            result.is_ok(),
            "list_zones must be callable without resident_mcp capability"
        );
    }

    #[test]
    fn test_list_scene_is_guest_accessible() {
        // list_scene is a guest tool.
        let (scene, _, _) = scene_with_zone();
        let result = handle_list_scene(json!(null), &scene);
        assert!(
            result.is_ok(),
            "list_scene must be callable without resident_mcp capability"
        );
    }

    // ── No shortcut tile-creation path for zone publishing ───────────────────

    #[test]
    fn test_publish_to_zone_latest_wins_no_tiles() {
        // LatestWins zone must never create tiles or nodes.
        let (mut scene, _, zone) = scene_with_zone();
        handle_publish_to_zone(
            json!({"zone_name": zone, "content": "# HUD content"}),
            &mut scene,
        )
        .unwrap();
        assert_eq!(
            scene.tile_count(),
            0,
            "LatestWins publish must not create tiles"
        );
        assert_eq!(
            scene.node_count(),
            0,
            "LatestWins publish must not create nodes"
        );
    }

    #[test]
    fn test_publish_to_zone_second_publish_no_additional_tiles() {
        // Even repeated publishing must never accumulate tiles.
        let (mut scene, _, zone) = scene_with_zone();
        for i in 1..=5u32 {
            handle_publish_to_zone(
                json!({"zone_name": zone, "content": format!("update-{i}")}),
                &mut scene,
            )
            .unwrap();
        }
        assert_eq!(
            scene.tile_count(),
            0,
            "repeated publishes must never create tiles (compositor resolves zone → tile)"
        );
    }

    #[test]
    fn test_create_tile_does_not_bypass_zone_policy() {
        // create_tile is a resident tool that creates a raw tile, NOT a zone publish.
        // It should not interfere with zone occupancy: publishing to a zone after
        // creating a raw tile must still see tile_count=1, zone publishes=1 separately.
        let (mut scene, tab_id) = scene_with_tab();
        let zone_name = "z2".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "test zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.0,
                    width_pct: 1.0,
                    height_pct: 0.1,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 4,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );

        // Create a raw tile (resident operation — simulated here at SceneGraph level).
        use tze_hud_scene::types::{Capability, Rect};
        let lease_id = scene.grant_lease(
            "resident-agent",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        scene
            .create_tile(
                tab_id,
                "resident-agent",
                lease_id,
                Rect::new(0.0, 0.0, 200.0, 100.0),
                1,
            )
            .unwrap();
        assert_eq!(
            scene.tile_count(),
            1,
            "raw tile must exist after create_tile"
        );

        // Now publish to zone — must not add more tiles.
        handle_publish_to_zone(
            json!({"zone_name": zone_name, "content": "zone content"}),
            &mut scene,
        )
        .unwrap();

        assert_eq!(
            scene.tile_count(),
            1,
            "zone publish must not add extra tiles on top of existing raw tiles"
        );
        // Zone must have a record, but it's distinct from the raw tile.
        assert!(
            scene
                .zone_registry
                .active_publishes
                .get(&zone_name)
                .is_some_and(|v| !v.is_empty()),
            "zone publish record must be created independently of raw tile"
        );
    }

    // ── publish_to_widget ─────────────────────────────────────────────────────

    /// Build a scene pre-populated with a "gauge" widget type and instance.
    fn scene_with_widget() -> (SceneGraph, SceneId) {
        use tze_hud_scene::types::{
            ContentionPolicy as CP, GeometryPolicy, RenderingPolicy, WidgetDefinition,
            WidgetInstance, WidgetParamType, WidgetParameterDeclaration, WidgetParameterValue,
        };

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).expect("create tab");

        // Register "gauge" widget type with one f32 param "level".
        scene.widget_registry.register_definition(WidgetDefinition {
            id: "gauge".to_string(),
            name: "Gauge".to_string(),
            description: "Gauge widget".to_string(),
            parameter_schema: vec![WidgetParameterDeclaration {
                name: "level".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: None,
            }],
            layers: vec![],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 0.2,
                height_pct: 0.1,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: CP::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: false,
            hover_behavior: None,
        });

        scene.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "gauge".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: "gauge".to_string(),
            current_params: std::collections::HashMap::new(),
        });

        (scene, tab_id)
    }

    #[test]
    fn test_publish_to_widget_missing_capability_rejected() {
        let (mut scene, _) = scene_with_widget();
        // No capabilities granted.
        let err = handle_publish_to_widget(
            json!({"widget_name": "gauge", "params": {"level": 0.5}}),
            &mut scene,
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(&err, McpError::SceneError(msg) if msg.contains("WIDGET_CAPABILITY_MISSING")),
            "expected WIDGET_CAPABILITY_MISSING, got: {err:?}"
        );
    }

    #[test]
    fn test_publish_to_widget_not_found() {
        let (mut scene, _) = scene_with_widget();
        let caps = vec!["publish_widget:nonexistent".to_string()];
        let err = handle_publish_to_widget(
            json!({"widget_name": "nonexistent", "params": {}}),
            &mut scene,
            &caps,
        )
        .unwrap_err();
        assert!(
            matches!(&err, McpError::SceneError(msg) if msg.contains("WIDGET_NOT_FOUND")),
            "expected WIDGET_NOT_FOUND, got: {err:?}"
        );
    }

    #[test]
    fn test_publish_to_widget_unknown_parameter() {
        let (mut scene, _) = scene_with_widget();
        let caps = vec!["publish_widget:gauge".to_string()];
        let err = handle_publish_to_widget(
            json!({"widget_name": "gauge", "params": {"bogus": 1.0}}),
            &mut scene,
            &caps,
        )
        .unwrap_err();
        // json_to_widget_param_value reports unknown param in its own message format.
        // handle_publish_to_widget does not run scene.publish_to_widget in this case
        // because the schema lookup fails first in json_to_widget_param_value.
        let msg = format!("{err:?}");
        assert!(
            msg.contains("WIDGET_UNKNOWN_PARAMETER") || msg.contains("not declared"),
            "expected unknown parameter error, got: {msg}"
        );
    }

    #[test]
    fn test_publish_to_widget_type_mismatch() {
        let (mut scene, _) = scene_with_widget();
        let caps = vec!["publish_widget:gauge".to_string()];
        // "level" is f32 but we pass a string.
        let err = handle_publish_to_widget(
            json!({"widget_name": "gauge", "params": {"level": "not-a-number"}}),
            &mut scene,
            &caps,
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("f32") || msg.contains("number") || msg.contains("type"),
            "expected type mismatch error, got: {msg}"
        );
    }

    #[test]
    fn test_publish_to_widget_durable_succeeds() {
        let (mut scene, _) = scene_with_widget();
        let caps = vec!["publish_widget:gauge".to_string()];
        let result = handle_publish_to_widget(
            json!({"widget_name": "gauge", "params": {"level": 0.75}}),
            &mut scene,
            &caps,
        )
        .unwrap();
        assert_eq!(result.widget_name, "gauge");
        assert!(result.durable, "gauge is a durable widget type");
        assert!(result.applied_params.contains(&"level".to_string()));
    }

    #[test]
    fn test_publish_to_widget_empty_params_succeeds() {
        // An empty params map is valid — zero fields to update is fine.
        let (mut scene, _) = scene_with_widget();
        let caps = vec!["publish_widget:gauge".to_string()];
        let result = handle_publish_to_widget(
            json!({"widget_name": "gauge", "params": {}}),
            &mut scene,
            &caps,
        )
        .unwrap();
        assert_eq!(result.widget_name, "gauge");
        assert!(result.applied_params.is_empty());
    }

    #[test]
    fn test_publish_to_widget_empty_widget_name_rejected() {
        let (mut scene, _) = scene_with_widget();
        let caps = vec!["publish_widget:gauge".to_string()];
        let err =
            handle_publish_to_widget(json!({"widget_name": "", "params": {}}), &mut scene, &caps)
                .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    // ── list_widgets ──────────────────────────────────────────────────────────

    #[test]
    fn test_list_widgets_empty_scene() {
        let scene = SceneGraph::new(1920.0, 1080.0);
        let result = handle_list_widgets(json!({}), &scene).unwrap();
        assert_eq!(result.type_count, 0);
        assert_eq!(result.instance_count, 0);
        assert!(result.widget_types.is_empty());
        assert!(result.widget_instances.is_empty());
    }

    #[test]
    fn test_list_widgets_returns_registered_type_and_instance() {
        let (scene, tab_id) = scene_with_widget();
        let result = handle_list_widgets(json!({}), &scene).unwrap();

        assert_eq!(result.type_count, 1);
        assert_eq!(result.instance_count, 1);

        let ty = &result.widget_types[0];
        assert_eq!(ty.id, "gauge");
        assert_eq!(ty.name, "Gauge");
        assert!(!ty.ephemeral);
        assert_eq!(ty.parameter_schema.len(), 1);
        assert_eq!(ty.parameter_schema[0].name, "level");
        assert_eq!(ty.parameter_schema[0].param_type, "f32");

        let inst = &result.widget_instances[0];
        assert_eq!(inst.instance_name, "gauge");
        assert_eq!(inst.widget_type, "gauge");
        assert_eq!(inst.tab_id, tab_id.to_string());
        assert!(inst.current_params.is_empty(), "no params published yet");
    }

    #[test]
    fn test_list_widgets_current_params_reflect_last_publish() {
        let (mut scene, _) = scene_with_widget();
        let caps = vec!["publish_widget:gauge".to_string()];
        handle_publish_to_widget(
            json!({"widget_name": "gauge", "params": {"level": 0.5}}),
            &mut scene,
            &caps,
        )
        .unwrap();

        let result = handle_list_widgets(json!({}), &scene).unwrap();
        let inst = &result.widget_instances[0];
        assert!(
            inst.current_params.contains_key("level"),
            "published param must appear in current_params"
        );
        // The f32 value 0.5 should be representable as a JSON number.
        let level_val = inst.current_params.get("level").unwrap();
        assert!(level_val.as_f64().is_some(), "level must be a JSON number");
    }

    #[test]
    fn test_list_widgets_is_guest_accessible() {
        // list_widgets must not require a resident capability — a guest-level
        // caller (no capabilities) must be able to call it without error.
        let (scene, _) = scene_with_widget();
        // No caller_capabilities needed — list_widgets takes no caps param.
        let result = handle_list_widgets(json!(null), &scene).unwrap();
        // Succeeds and returns data — confirms guest access works.
        assert_eq!(result.type_count, 1);
    }

    #[test]
    fn test_publish_to_widget_is_capability_gated() {
        // publish_to_widget without the right capability → WIDGET_CAPABILITY_MISSING.
        // Confirms the tool performs its own gate and does not require a separate
        // outer access check (i.e., the guest classification is correct: the tool
        // handles its own capability gating internally).
        let (mut scene, _) = scene_with_widget();
        let err = handle_publish_to_widget(
            json!({"widget_name": "gauge", "params": {"level": 0.5}}),
            &mut scene,
            &[], // no capabilities
        )
        .unwrap_err();
        assert!(
            matches!(&err, McpError::SceneError(m) if m.contains("WIDGET_CAPABILITY_MISSING")),
            "expected WIDGET_CAPABILITY_MISSING, got: {err:?}"
        );
    }

    // ── clear_widget ──────────────────────────────────────────────────────────

    #[test]
    fn test_clear_widget_removes_own_publications() {
        let (mut scene, _) = scene_with_widget();
        let caps = vec!["publish_widget:gauge".to_string()];

        // Publish first
        handle_publish_to_widget(
            json!({"widget_name": "gauge", "namespace": "agent.a", "params": {"level": 0.8}}),
            &mut scene,
            &caps,
        )
        .unwrap();
        assert_eq!(scene.widget_registry.active_for_widget("gauge").len(), 1);

        // Clear
        let result = handle_clear_widget(
            json!({"widget_name": "gauge", "namespace": "agent.a"}),
            &mut scene,
            &caps,
        )
        .unwrap();
        assert_eq!(result.widget_name, "gauge");
        assert!(result.cleared);
        assert!(result.changed);
        assert_eq!(
            scene.widget_registry.active_for_widget("gauge").len(),
            0,
            "agent.a's publication should be cleared"
        );
    }

    #[test]
    fn test_clear_widget_missing_capability_rejected() {
        let (mut scene, _) = scene_with_widget();
        let err = handle_clear_widget(
            json!({"widget_name": "gauge", "namespace": "agent.a"}),
            &mut scene,
            &[], // no capabilities
        )
        .unwrap_err();
        assert!(
            matches!(&err, McpError::SceneError(m) if m.contains("WIDGET_CAPABILITY_MISSING")),
            "expected WIDGET_CAPABILITY_MISSING, got: {err:?}"
        );
    }

    #[test]
    fn test_clear_widget_not_found() {
        let (mut scene, _) = scene_with_widget();
        let caps = vec!["publish_widget:nonexistent".to_string()];
        let err = handle_clear_widget(
            json!({"widget_name": "nonexistent", "namespace": "agent.a"}),
            &mut scene,
            &caps,
        )
        .unwrap_err();
        assert!(
            matches!(&err, McpError::SceneError(m) if m.contains("WIDGET_NOT_FOUND")),
            "expected WIDGET_NOT_FOUND, got: {err:?}"
        );
    }

    #[test]
    fn test_clear_widget_empty_name_rejected() {
        let (mut scene, _) = scene_with_widget();
        let caps = vec!["publish_widget:gauge".to_string()];
        let err = handle_clear_widget(
            json!({"widget_name": "", "namespace": "agent.a"}),
            &mut scene,
            &caps,
        )
        .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    #[test]
    fn test_clear_widget_noop_when_no_publications() {
        // clear_widget with no prior publications should succeed silently.
        let (mut scene, _) = scene_with_widget();
        let caps = vec!["publish_widget:gauge".to_string()];
        let result = handle_clear_widget(
            json!({"widget_name": "gauge", "namespace": "agent.nobody"}),
            &mut scene,
            &caps,
        )
        .unwrap();
        assert!(result.cleared);
        assert!(!result.changed);
    }

    // ── register_widget_asset ────────────────────────────────────────────────

    #[test]
    fn test_register_widget_asset_preflight_dedup_hit() {
        let mut registry = WidgetAssetRegistry::default();
        let caps = vec!["register_widget_asset".to_string()];
        let payload =
            r#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="10" height="10"/></svg>"#;
        let hash_hex = bytes_to_hex(blake3::hash(payload.as_bytes()).as_bytes());

        let first = handle_register_widget_asset(
            json!({
                "widget_type_id": "gauge",
                "svg_filename": "face.svg",
                "content_hash_blake3": hash_hex,
                "total_size_bytes": payload.len(),
                "payload": payload
            }),
            &mut registry,
            &caps,
        )
        .unwrap();
        assert!(first.accepted);
        assert!(!first.was_deduplicated);
        assert!(first.asset_handle.is_some());
        let hash_raw = parse_blake3_hex_32(&hash_hex).unwrap();
        assert_eq!(
            registry
                .get_by_hash(&hash_raw)
                .map(|entry| entry.asset_handle.as_str()),
            first.asset_handle.as_deref()
        );

        let preflight = handle_register_widget_asset(
            json!({
                "widget_type_id": "gauge",
                "svg_filename": "face.svg",
                "content_hash_blake3": hash_hex,
                "total_size_bytes": payload.len(),
                "metadata_only_preflight": true
            }),
            &mut registry,
            &caps,
        )
        .unwrap();
        assert!(preflight.accepted);
        assert!(preflight.was_deduplicated);
        assert!(preflight.asset_handle.is_some());
    }

    #[test]
    fn test_register_widget_asset_capability_missing() {
        let mut registry = WidgetAssetRegistry::default();
        let result = handle_register_widget_asset(
            json!({
                "widget_type_id": "gauge",
                "svg_filename": "face.svg",
                "content_hash_blake3": "00".repeat(32),
                "total_size_bytes": 0
            }),
            &mut registry,
            &[],
        )
        .unwrap();
        assert!(!result.accepted);
        assert_eq!(
            result.error_code.as_deref(),
            Some("WIDGET_ASSET_CAPABILITY_MISSING")
        );
    }

    #[test]
    fn test_register_widget_asset_checksum_mismatch() {
        let mut registry = WidgetAssetRegistry::default();
        let caps = vec!["register_widget_asset".to_string()];
        let payload =
            r#"<svg xmlns="http://www.w3.org/2000/svg"><circle r="4" cx="5" cy="5"/></svg>"#;
        let hash_hex = bytes_to_hex(blake3::hash(payload.as_bytes()).as_bytes());

        let result = handle_register_widget_asset(
            json!({
                "widget_type_id": "gauge",
                "svg_filename": "dial.svg",
                "content_hash_blake3": hash_hex,
                "total_size_bytes": payload.len(),
                "transport_crc32c": 42,
                "payload": payload
            }),
            &mut registry,
            &caps,
        )
        .unwrap();
        assert!(!result.accepted);
        assert_eq!(
            result.error_code.as_deref(),
            Some("WIDGET_ASSET_CHECKSUM_MISMATCH")
        );
    }

    #[test]
    fn test_register_widget_asset_invalid_svg() {
        let mut registry = WidgetAssetRegistry::default();
        let caps = vec!["register_widget_asset".to_string()];
        let payload = "<not-svg />";
        let hash_hex = bytes_to_hex(blake3::hash(payload.as_bytes()).as_bytes());

        let result = handle_register_widget_asset(
            json!({
                "widget_type_id": "gauge",
                "svg_filename": "broken.svg",
                "content_hash_blake3": hash_hex,
                "total_size_bytes": payload.len(),
                "payload": payload
            }),
            &mut registry,
            &caps,
        )
        .unwrap();
        assert!(!result.accepted);
        assert_eq!(
            result.error_code.as_deref(),
            Some("WIDGET_ASSET_INVALID_SVG")
        );
    }

    #[test]
    fn test_register_widget_asset_metadata_preflight_miss() {
        let mut registry = WidgetAssetRegistry::default();
        let caps = vec!["register_widget_asset".to_string()];
        let result = handle_register_widget_asset(
            json!({
                "widget_type_id": "gauge",
                "svg_filename": "face.svg",
                "content_hash_blake3": "11".repeat(32),
                "total_size_bytes": 12,
                "metadata_only_preflight": true
            }),
            &mut registry,
            &caps,
        )
        .unwrap();
        assert!(!result.accepted);
        assert_eq!(
            result.error_code.as_deref(),
            Some("WIDGET_ASSET_HASH_MISMATCH")
        );
    }

    #[test]
    fn test_register_widget_asset_accepts_prefixed_svg_root() {
        let mut registry = WidgetAssetRegistry::default();
        let caps = vec!["register_widget_asset".to_string()];
        let payload = r#"<svg:svg xmlns:svg="http://www.w3.org/2000/svg"><svg:rect width="10" height="10"/></svg:svg>"#;
        let hash_hex = bytes_to_hex(blake3::hash(payload.as_bytes()).as_bytes());
        let result = handle_register_widget_asset(
            json!({
                "widget_type_id": "gauge",
                "svg_filename": "prefixed.svg",
                "content_hash_blake3": hash_hex,
                "total_size_bytes": payload.len(),
                "payload": payload
            }),
            &mut registry,
            &caps,
        )
        .unwrap();
        assert!(result.accepted);
        assert!(!result.was_deduplicated);
    }

    #[test]
    fn test_register_widget_asset_registry_capacity_limit() {
        let mut registry = WidgetAssetRegistry::default();
        let caps = vec!["register_widget_asset".to_string()];
        for i in 0..WidgetAssetRegistry::MAX_ENTRIES {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&(i as u64).to_le_bytes());
            registry.upsert(hash, format!("widget-asset:{i}"));
        }

        let payload =
            r#"<svg xmlns="http://www.w3.org/2000/svg"><circle cx="1" cy="1" r="1"/></svg>"#;
        let hash_hex = bytes_to_hex(blake3::hash(payload.as_bytes()).as_bytes());
        let result = handle_register_widget_asset(
            json!({
                "widget_type_id": "gauge",
                "svg_filename": "full.svg",
                "content_hash_blake3": hash_hex,
                "total_size_bytes": payload.len(),
                "payload": payload
            }),
            &mut registry,
            &caps,
        )
        .unwrap();

        assert!(!result.accepted);
        assert_eq!(
            result.error_code.as_deref(),
            Some("WIDGET_ASSET_BUDGET_EXCEEDED")
        );
    }

    // ── parse_zone_content: static_image ────────────────────────────────────

    /// Build a scene with a zone that accepts StaticImage media type.
    fn scene_with_static_image_zone() -> (SceneGraph, String) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let zone_name = "pip".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "Picture-in-picture zone (accepts static images)".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.0,
                    width_pct: 0.25,
                    height_pct: 0.25,
                },
                accepted_media_types: vec![ZoneMediaType::StaticImage],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::Replace,
                max_publishers: 1,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );
        (scene, zone_name)
    }

    #[test]
    fn test_parse_zone_content_static_image_valid_hex() {
        // A 64-char lowercase hex string must be accepted and produce StaticImage.
        let (mut scene, zone) = scene_with_static_image_zone();
        // blake3::hash(b"test") as a 64-char hex string.
        let hex = "4878ca0425c739fa427f7eda20fe845f6b2f46ba5fe5ac7d6b85add8db6bb08f"; // blake3 of "test"
        let result = handle_publish_to_zone(
            json!({"zone_name": zone, "content": {"type": "static_image", "resource_id": hex}}),
            &mut scene,
        )
        .unwrap();
        assert_eq!(result.zone_name, zone);
        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(publishes.len(), 1);
        // Verify the correct variant was produced and that the hex was decoded correctly.
        if let tze_hud_scene::types::ZoneContent::StaticImage(resource_id) = &publishes[0].content {
            let expected: [u8; 32] = [
                0x48, 0x78, 0xca, 0x04, 0x25, 0xc7, 0x39, 0xfa, 0x42, 0x7f, 0x7e, 0xda, 0x20, 0xfe,
                0x84, 0x5f, 0x6b, 0x2f, 0x46, 0xba, 0x5f, 0xe5, 0xac, 0x7d, 0x6b, 0x85, 0xad, 0xd8,
                0xdb, 0x6b, 0xb0, 0x8f,
            ];
            assert_eq!(
                resource_id.as_bytes(),
                &expected,
                "parsed ResourceId bytes must match expected value for provided hex"
            );
        } else {
            panic!(
                "static_image content must produce ZoneContent::StaticImage, got: {:?}",
                &publishes[0].content
            );
        }
    }

    #[test]
    fn test_parse_zone_content_static_image_uppercase_hex_accepted() {
        // Uppercase hex must also be accepted.
        let (mut scene, zone) = scene_with_static_image_zone();
        let hex = "4878CA0425C739FA427F7EDA20FE845F6B2F46BA5FE5AC7D6B85ADD8DB6BB08F";
        let result = handle_publish_to_zone(
            json!({"zone_name": zone, "content": {"type": "static_image", "resource_id": hex}}),
            &mut scene,
        );
        // Uppercase hex is valid (A-F are recognized by to_digit(16)).
        assert!(result.is_ok(), "uppercase hex must be accepted: {result:?}");
    }

    #[test]
    fn test_parse_zone_content_static_image_missing_resource_id_rejected() {
        // Missing resource_id field must return InvalidParams.
        let (mut scene, zone) = scene_with_static_image_zone();
        let err = handle_publish_to_zone(
            json!({"zone_name": zone, "content": {"type": "static_image"}}),
            &mut scene,
        )
        .unwrap_err();
        assert!(
            matches!(&err, McpError::InvalidParams(msg) if msg.contains("resource_id")),
            "expected InvalidParams about resource_id, got: {err:?}"
        );
    }

    #[test]
    fn test_parse_zone_content_static_image_wrong_length_rejected() {
        // A hex string that is not exactly 64 chars must return InvalidParams.
        let (mut scene, zone) = scene_with_static_image_zone();
        let err = handle_publish_to_zone(
            json!({"zone_name": zone, "content": {"type": "static_image", "resource_id": "deadbeef"}}),
            &mut scene,
        )
        .unwrap_err();
        assert!(
            matches!(&err, McpError::InvalidParams(msg) if msg.contains("64 hex chars") || msg.contains("64")),
            "expected InvalidParams about length, got: {err:?}"
        );
    }

    #[test]
    fn test_parse_zone_content_static_image_invalid_hex_chars_rejected() {
        // Non-hex characters must return InvalidParams.
        let (mut scene, zone) = scene_with_static_image_zone();
        // 64 chars with "XXXX" at the end, which are not valid hex digits.
        let bad_hex = "4878ca0425c739fa427f7eda20fe845f6b2f46ba5fe5ac7d6b85add8db6bXXXX";
        let err = handle_publish_to_zone(
            json!({"zone_name": zone, "content": {"type": "static_image", "resource_id": bad_hex}}),
            &mut scene,
        )
        .unwrap_err();
        assert!(
            matches!(&err, McpError::InvalidParams(msg) if msg.contains("not valid hex") || msg.contains("hex")),
            "expected InvalidParams about invalid hex, got: {err:?}"
        );
    }

    #[test]
    fn test_parse_zone_content_unknown_type_error_includes_static_image() {
        // The error message for unknown content type must list static_image as valid.
        let (mut scene, zone) = scene_with_static_image_zone();
        let err = handle_publish_to_zone(
            json!({"zone_name": zone, "content": {"type": "bogus_type"}}),
            &mut scene,
        )
        .unwrap_err();
        assert!(
            matches!(&err, McpError::InvalidParams(msg) if msg.contains("static_image")),
            "error for unknown type must mention static_image, got: {err:?}"
        );
    }

    // ── Notification stack exemplar — MCP integration tests ─────────────────
    //
    // These 5 tests exercise the notification-area zone (max_depth=5,
    // auto_clear_ms=8000, Stack contention policy) via the MCP
    // `publish_to_zone` path, verifying the scenarios required by
    // openspec/changes/exemplar-notification/specs/exemplar-notification/spec.md
    // §Requirement: Notification Exemplar MCP Integration Test.

    /// Build a scene with the canonical notification-area zone (max_depth=5,
    /// ShortTextWithIcon, Stack contention, auto_clear_ms=8000).
    fn scene_with_notification_area() -> (SceneGraph, String) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let zone_name = "notification-area".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "Notification overlay area".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.75,
                    y_pct: 0.02,
                    width_pct: 0.24,
                    height_pct: 0.30,
                },
                accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::Stack { max_depth: 5 },
                max_publishers: 16,
                transport_constraint: None,
                auto_clear_ms: Some(8_000),
                ephemeral: false,
                layer_attachment: LayerAttachment::Chrome,
            },
        );
        (scene, zone_name)
    }

    /// Build a notification-area-backed scene using an injectable TestClock.
    ///
    /// The TestClock allows deterministic TTL expiry tests without real sleeps.
    fn scene_with_notification_area_and_clock() -> (SceneGraph, tze_hud_scene::TestClock, String) {
        use std::sync::Arc;
        let clock = tze_hud_scene::TestClock::new(1_000); // start at 1 second
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let zone_name = "notification-area".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "Notification overlay area".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.75,
                    y_pct: 0.02,
                    width_pct: 0.24,
                    height_pct: 0.30,
                },
                accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::Stack { max_depth: 5 },
                max_publishers: 16,
                transport_constraint: None,
                auto_clear_ms: Some(8_000),
                ephemeral: false,
                layer_attachment: LayerAttachment::Chrome,
            },
        );
        (scene, clock, zone_name)
    }

    /// Test 1: Multi-agent stack ordering.
    ///
    /// Three agents publish notifications to the notification-area zone.
    /// Verifies that:
    /// - All 3 records accumulate in active_publishes (Stack policy).
    /// - Arrival order is preserved: alpha first (index 0), then beta, then gamma (index 2, newest).
    /// - The newest publication (gamma) is at the end of the Vec (rendered at top per spec).
    #[test]
    fn test_notification_stack_multi_agent_arrival_order() {
        let (mut scene, zone) = scene_with_notification_area();

        // Three agents publish in order: alpha → beta → gamma.
        handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "namespace": "alpha",
                "content": {"type": "notification", "text": "System idle", "icon": "", "urgency": 0}
            }),
            &mut scene,
        )
        .unwrap();
        handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "namespace": "beta",
                "content": {"type": "notification", "text": "Update available", "icon": "update", "urgency": 1}
            }),
            &mut scene,
        )
        .unwrap();
        handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "namespace": "gamma",
                "content": {"type": "notification", "text": "Security alert", "icon": "shield", "urgency": 3}
            }),
            &mut scene,
        )
        .unwrap();

        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            publishes.len(),
            3,
            "all 3 agent notifications must be present in the stack"
        );

        // Slot assignment: oldest at index 0, newest at index 2 (rendered at top).
        // Spec §Three notifications stack vertically newest-on-top: gamma (newest) at top.
        if let tze_hud_scene::types::ZoneContent::Notification(n) = &publishes[0].content {
            assert_eq!(
                n.text, "System idle",
                "alpha (oldest) must be at slot index 0"
            );
        } else {
            panic!(
                "expected Notification at index 0, got {:?}",
                &publishes[0].content
            );
        }
        if let tze_hud_scene::types::ZoneContent::Notification(n) = &publishes[2].content {
            assert_eq!(
                n.text, "Security alert",
                "gamma (newest) must be at slot index 2"
            );
        } else {
            panic!(
                "expected Notification at index 2, got {:?}",
                &publishes[2].content
            );
        }

        // Publisher namespaces must reflect each agent's identity.
        assert_eq!(publishes[0].publisher_namespace, "alpha");
        assert_eq!(publishes[1].publisher_namespace, "beta");
        assert_eq!(publishes[2].publisher_namespace, "gamma");
    }

    /// Test 2: Max depth eviction.
    ///
    /// Publishing 6 notifications to a max_depth=5 zone must evict the oldest
    /// (first) record immediately with no fade-out, leaving exactly 5 records.
    /// Spec §Sixth notification evicts oldest and §Evicted notification has no fade-out.
    #[test]
    fn test_notification_stack_max_depth_eviction() {
        let (mut scene, zone) = scene_with_notification_area();

        // Publish 6 notifications from 6 distinct agents.
        for i in 1..=6u32 {
            handle_publish_to_zone(
                json!({
                    "zone_name": zone,
                    "namespace": format!("agent-{i}"),
                    "content": {
                        "type": "notification",
                        "text": format!("notification-{i}"),
                        "icon": "",
                        "urgency": 1
                    }
                }),
                &mut scene,
            )
            .unwrap();
        }

        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            publishes.len(),
            5,
            "max_depth=5: only 5 newest notifications must remain after 6th publish"
        );

        // The oldest (notification-1) must be evicted.
        let has_first = publishes.iter().any(|r| {
            matches!(&r.content, tze_hud_scene::types::ZoneContent::Notification(n) if n.text == "notification-1")
        });
        assert!(!has_first, "notification-1 (oldest) must be evicted");

        // The newest (notification-6) must be present at the end.
        let last = &publishes[4];
        if let tze_hud_scene::types::ZoneContent::Notification(n) = &last.content {
            assert_eq!(
                n.text, "notification-6",
                "notification-6 (newest) must be at end"
            );
        } else {
            panic!("expected Notification at index 4, got {:?}", last.content);
        }

        // The 5 surviving records must be notifications 2 through 6.
        let texts: Vec<&str> = publishes
            .iter()
            .filter_map(|r| {
                if let tze_hud_scene::types::ZoneContent::Notification(n) = &r.content {
                    Some(n.text.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                "notification-2",
                "notification-3",
                "notification-4",
                "notification-5",
                "notification-6"
            ],
            "surviving records must be 2-6 in arrival order"
        );
    }

    /// Test 3: TTL auto-dismiss.
    ///
    /// A notification published with urgency=0 receives an auto-dismiss expiry of
    /// NOTIFICATION_TTL_INFO_US (8 s) from the scene graph. Advancing the clock
    /// past that expiry and calling drain_expired_zone_publications must remove it.
    ///
    /// Spec §Notification auto-dismisses after 8 seconds and
    ///      §Notification removed after fade-out completes.
    #[test]
    fn test_notification_stack_ttl_auto_dismiss() {
        let (mut scene, clock, zone) = scene_with_notification_area_and_clock();

        // Publish a low-urgency notification (urgency=0 → 8s auto-dismiss).
        handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "namespace": "agent-ttl",
                "content": {
                    "type": "notification",
                    "text": "Will expire",
                    "icon": "",
                    "urgency": 0
                }
            }),
            &mut scene,
        )
        .unwrap();

        // Confirm publication is present before expiry.
        let before = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            before.len(),
            1,
            "notification must be present before TTL expires"
        );
        assert!(
            before[0].expires_at_wall_us.is_some(),
            "urgency=0 notification must have an auto-dismiss expiry set"
        );

        // Advance clock to just before expiry — publication must still be present.
        clock.advance(SceneGraph::NOTIFICATION_TTL_INFO_US / 1_000 - 1);
        let removed_early = scene.drain_expired_zone_publications();
        assert_eq!(
            removed_early, 0,
            "publication must not be removed before TTL expires"
        );
        let still_present = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            still_present.len(),
            1,
            "notification must still be present 1ms before TTL"
        );

        // Advance clock 2ms past expiry (now = 1000 + 8000 + 1ms = past the TTL boundary).
        // drain_expired_zone_publications removes records as soon as expires_at_wall_us <= now_us;
        // no fade state is tracked at the scene-graph layer (fade is a compositor concern).
        clock.advance(2);
        let removed = scene.drain_expired_zone_publications();
        assert_eq!(
            removed, 1,
            "one notification must be removed after TTL expires"
        );

        // Zone must have no active publications.
        let after = scene.zone_registry.active_publishes.get(&zone);
        assert!(
            after.is_none() || after.unwrap().is_empty(),
            "notification-area must be empty after TTL expiry and drain"
        );
    }

    /// Test 4: Urgency backdrop colors.
    ///
    /// Publishes notifications with urgency 0, 2, and 3, and verifies that the
    /// urgency values are correctly preserved in active_publishes (the renderer
    /// maps urgency → backdrop color; the MCP/scene layer preserves urgency as-is).
    ///
    /// Also verifies that urgency=5 is stored in the payload without error (clamping
    /// to critical=3 is the compositor's responsibility at render time, not the scene
    /// graph's, so urgency=5 is accepted and stored unchanged).
    ///
    /// Spec §Urgency-Tinted Notification Backdrops and §Out-of-range urgency clamped to critical.
    #[test]
    fn test_notification_stack_urgency_backdrop_colors() {
        let (mut scene, zone) = scene_with_notification_area();

        // Publish urgency 0 (low → #2A2A2A backdrop), urgency 2 (urgent → #8B6914),
        // urgency 3 (critical → #8B1A1A), and urgency 5 (out-of-range → clamped to critical
        // at render time; stored as 5 in the record).
        for (ns, urgency, label) in &[
            ("agent-low", 0u32, "low"),
            ("agent-urgent", 2u32, "urgent"),
            ("agent-critical", 3u32, "critical"),
            ("agent-oob", 5u32, "out-of-range"),
        ] {
            handle_publish_to_zone(
                json!({
                    "zone_name": zone,
                    "namespace": ns,
                    "content": {
                        "type": "notification",
                        "text": format!("urgency-{label}"),
                        "icon": "",
                        "urgency": urgency
                    }
                }),
                &mut scene,
            )
            .unwrap_or_else(|e| panic!("publish urgency={urgency} failed: {e:?}"));
        }

        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            publishes.len(),
            4,
            "all 4 urgency-level notifications must be present"
        );

        // Verify each urgency value is stored unchanged in the payload.
        let urgency_by_ns: std::collections::HashMap<&str, u32> = publishes
            .iter()
            .filter_map(|r| {
                if let tze_hud_scene::types::ZoneContent::Notification(n) = &r.content {
                    Some((r.publisher_namespace.as_str(), n.urgency))
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(
            urgency_by_ns["agent-low"], 0,
            "urgency=0 (low) must be preserved"
        );
        assert_eq!(
            urgency_by_ns["agent-urgent"], 2,
            "urgency=2 (urgent) must be preserved"
        );
        assert_eq!(
            urgency_by_ns["agent-critical"], 3,
            "urgency=3 (critical) must be preserved"
        );
        // urgency=5 is stored as-is; the compositor clamps it to 3 at render time.
        assert_eq!(
            urgency_by_ns["agent-oob"], 5,
            "urgency=5 (out-of-range) must be stored as-is in the record; compositor clamps to critical=3"
        );
    }

    /// Test 5: Agent independence.
    ///
    /// When one agent's notification TTL expires and is drained, the other agent's
    /// notification must remain unaffected in the stack.
    ///
    /// Spec §Agents do not interfere with each other's notifications.
    #[test]
    fn test_notification_stack_agent_independence() {
        let (mut scene, clock, zone) = scene_with_notification_area_and_clock();

        // Agent-alpha publishes urgency=0 (8s TTL → expires at now+8000ms).
        handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "namespace": "agent-alpha",
                "content": {
                    "type": "notification",
                    "text": "Alpha message",
                    "icon": "",
                    "urgency": 0
                }
            }),
            &mut scene,
        )
        .unwrap();

        // Advance 1ms so beta's published_at_wall_us is distinct from alpha's.
        clock.advance(1);

        // Agent-beta publishes urgency=3 (critical → 30s TTL → expires at now+30000ms).
        handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "namespace": "agent-beta",
                "content": {
                    "type": "notification",
                    "text": "Beta message",
                    "icon": "",
                    "urgency": 3
                }
            }),
            &mut scene,
        )
        .unwrap();

        // Both notifications must be present before any TTL expires.
        let before = scene.zone_registry.active_for_zone(&zone);
        assert_eq!(
            before.len(),
            2,
            "both notifications must be active before any expiry"
        );

        // Advance clock past alpha's 8s TTL (urgency=0 → NOTIFICATION_TTL_INFO_US).
        // Beta's 30s TTL (urgency=3 → NOTIFICATION_TTL_CRITICAL_US) must not have expired.
        clock.advance(SceneGraph::NOTIFICATION_TTL_INFO_US / 1_000 + 500); // +8500ms
        let removed = scene.drain_expired_zone_publications();
        assert_eq!(
            removed, 1,
            "only alpha's notification must be removed at t=8500ms"
        );

        // Beta's notification must remain unaffected.
        let after = scene.zone_registry.active_for_zone(&zone);
        assert_eq!(
            after.len(),
            1,
            "beta's notification must survive alpha's TTL expiry"
        );
        if let tze_hud_scene::types::ZoneContent::Notification(n) = &after[0].content {
            assert_eq!(
                n.text, "Beta message",
                "surviving notification must be beta's"
            );
            assert_eq!(
                after[0].publisher_namespace, "agent-beta",
                "surviving record must belong to agent-beta"
            );
        } else {
            panic!("expected Notification, got {:?}", after[0].content);
        }
    }

    // ── Streaming breakpoint reveal — MCP path (hud-hzub.4) ─────────────────

    /// Build a scene with a subtitle zone (LatestWins, StreamText-only).
    /// Used by streaming reveal and list_zones subtitle tests.
    fn scene_with_subtitle_zone() -> (SceneGraph, String) {
        let (mut scene, _) = scene_with_tab();
        let zone_name = "subtitle".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "Subtitle overlay".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.1,
                    y_pct: 0.85,
                    width_pct: 0.8,
                    height_pct: 0.10,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 2,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );
        (scene, zone_name)
    }

    /// MCP publish_to_zone with stream_text content containing breakpoints:
    /// verify breakpoint indices are forwarded to the compositor.
    ///
    /// Spec §Subtitle Streaming Word-by-Word Reveal:
    /// "The compositor MUST reveal the text progressively: first "The", then
    ///  "The quick", then "The quick brown", then "The quick brown fox"."
    #[test]
    fn test_mcp_publish_to_zone_with_breakpoints_forwarded_to_record() {
        let (mut scene, zone) = scene_with_subtitle_zone();
        // "The quick brown fox" — breakpoints at word boundaries
        // byte offsets: after "The"=3, after "quick"=9, after "brown"=15
        let result = handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "content": "The quick brown fox",
                "breakpoints": [3, 9, 15],
                "namespace": "exemplar-test"
            }),
            &mut scene,
        )
        .unwrap();
        assert_eq!(result.zone_name, zone);

        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(publishes.len(), 1);
        // Content must be StreamText
        assert!(
            matches!(&publishes[0].content, tze_hud_scene::types::ZoneContent::StreamText(s) if s == "The quick brown fox"),
            "content must be StreamText"
        );
        // Breakpoints must be forwarded to the publish record
        assert_eq!(
            publishes[0].breakpoints,
            vec![3u64, 9, 15],
            "breakpoints must be forwarded to the ZonePublishRecord for compositor reveal"
        );
    }

    /// MCP publish_to_zone with stream_text via object syntax: verify breakpoints work
    /// with the {"type":"stream_text","text":"..."} content form too.
    #[test]
    fn test_mcp_publish_to_zone_object_stream_text_with_breakpoints() {
        let (mut scene, zone) = scene_with_subtitle_zone();
        let result = handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "content": {"type": "stream_text", "text": "The quick brown fox"},
                "breakpoints": [3, 9, 15],
                "namespace": "exemplar-test"
            }),
            &mut scene,
        )
        .unwrap();
        assert_eq!(result.zone_name, zone);

        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(publishes[0].breakpoints, vec![3u64, 9, 15]);
    }

    /// MCP publish_to_zone without breakpoints (empty array) must reveal all text immediately.
    ///
    /// Spec §"Stream-text without breakpoints reveals all at once":
    /// "THEN the compositor MUST display the full text immediately (no progressive reveal)."
    #[test]
    fn test_mcp_publish_to_zone_empty_breakpoints_reveals_immediately() {
        let (mut scene, zone) = scene_with_subtitle_zone();
        handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "content": "Instant display",
                "breakpoints": [],
                "namespace": "exemplar-test"
            }),
            &mut scene,
        )
        .unwrap();

        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(publishes.len(), 1);
        assert!(
            publishes[0].breakpoints.is_empty(),
            "empty breakpoints must result in empty breakpoints in the publish record"
        );
    }

    /// MCP publish_to_zone without breakpoints field at all — same as empty (default).
    #[test]
    fn test_mcp_publish_to_zone_no_breakpoints_field_defaults_empty() {
        let (mut scene, zone) = scene_with_subtitle_zone();
        handle_publish_to_zone(
            json!({"zone_name": zone, "content": "Hello world", "namespace": "exemplar-test"}),
            &mut scene,
        )
        .unwrap();

        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert!(
            publishes[0].breakpoints.is_empty(),
            "absent breakpoints field must default to empty"
        );
    }

    /// Replacement during streaming cancels reveal — latest-wins applies.
    ///
    /// Spec §"Replacement during streaming cancels reveal":
    /// "THEN the compositor MUST cancel the in-progress reveal and display the new content."
    /// At the scene layer, latest-wins replaces the previous publish record (and its breakpoints).
    #[test]
    fn test_mcp_publish_to_zone_replacement_cancels_breakpoints() {
        let (mut scene, zone) = scene_with_subtitle_zone();

        // First publish: streaming with breakpoints
        handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "content": "Long streaming message",
                "breakpoints": [4, 13],
                "namespace": "exemplar-test"
            }),
            &mut scene,
        )
        .unwrap();

        // Second publish replaces first — latest-wins semantics
        handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "content": "Replacement content",
                "namespace": "exemplar-test"
            }),
            &mut scene,
        )
        .unwrap();

        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            publishes.len(),
            1,
            "LatestWins must have only one active record"
        );
        assert!(
            matches!(&publishes[0].content, tze_hud_scene::types::ZoneContent::StreamText(s) if s == "Replacement content"),
            "replacement content must be the active record"
        );
        assert!(
            publishes[0].breakpoints.is_empty(),
            "replacement without breakpoints must clear breakpoints (no streaming for new content)"
        );
    }

    /// Breakpoints rejected for non-StreamText content.
    #[test]
    fn test_mcp_publish_to_zone_breakpoints_rejected_for_non_stream_text() {
        let (mut scene, _) = scene_with_tab();
        // Set up a notification zone
        let zone_name = "notification-area".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "Notification zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.75,
                    y_pct: 0.0,
                    width_pct: 0.25,
                    height_pct: 0.30,
                },
                accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::Stack { max_depth: 3 },
                max_publishers: 8,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );
        let err = handle_publish_to_zone(
            json!({
                "zone_name": zone_name,
                "content": {"type": "notification", "text": "Alert!", "icon": "", "urgency": 1},
                "breakpoints": [3, 9],
                "namespace": "exemplar-test"
            }),
            &mut scene,
        )
        .unwrap_err();
        assert!(
            matches!(err, McpError::InvalidParams(_)),
            "breakpoints on non-StreamText content must be rejected with InvalidParams"
        );
    }

    // ── list_zones subtitle zone metadata (hud-hzub.4) ──────────────────────

    /// list_zones reports subtitle zone with contention_policy: latest_wins.
    ///
    /// Spec §Subtitle Contention Policy — Latest Wins.
    #[test]
    fn test_list_zones_subtitle_contention_policy_latest_wins() {
        let (scene, zone) = scene_with_subtitle_zone();
        let result = handle_list_zones(json!(null), &scene).unwrap();
        let entry = result.zones.iter().find(|z| z.name == zone).unwrap();
        assert_eq!(
            entry.contention_policy, "latest_wins",
            "subtitle zone must report contention_policy = latest_wins"
        );
    }

    /// list_zones reports subtitle zone with accepted_media_types including stream_text.
    ///
    /// Spec §Subtitle MCP Test Fixtures — zone_name: "subtitle".
    #[test]
    fn test_list_zones_subtitle_accepted_media_types_includes_stream_text() {
        let (scene, zone) = scene_with_subtitle_zone();
        let result = handle_list_zones(json!(null), &scene).unwrap();
        let entry = result.zones.iter().find(|z| z.name == zone).unwrap();
        assert!(
            entry
                .accepted_media_types
                .contains(&"stream_text".to_string()),
            "subtitle zone must include stream_text in accepted_media_types, got {:?}",
            entry.accepted_media_types
        );
    }

    /// list_zones exposes contention_policy and accepted_media_types for all zone types.
    #[test]
    fn test_list_zones_exposes_contention_policy_and_media_types() {
        let (scene, _tab_id, zone) = scene_with_zone(); // main-overlay: LatestWins, StreamText
        let result = handle_list_zones(json!(null), &scene).unwrap();
        let entry = result.zones.iter().find(|z| z.name == zone).unwrap();
        assert_eq!(entry.contention_policy, "latest_wins");
        assert!(
            entry
                .accepted_media_types
                .contains(&"stream_text".to_string())
        );
    }

    // ── Ambient-background MCP integration tests ─────────────────────────────
    //
    // These 5 tests exercise the ambient-background zone (Replace contention,
    // Background layer, accepts SolidColor + StaticImage) via the MCP
    // `publish_to_zone` path, verifying the scenarios required by
    // openspec/changes/exemplar-ambient-background/specs/exemplar-ambient-background/spec.md
    // §Requirements: "MCP StaticImage Content Type Dispatch" and
    //                "Ambient Background User-Test Scenarios".
    //
    // Task references: hud-gwhr.4, tasks.md §4 (user-test integration),
    //                  tasks.md §1.4 and §1.5 (MCP static_image tests).

    /// Build a scene with the canonical ambient-background zone
    /// (full-screen, Replace, Background layer, SolidColor + StaticImage).
    fn scene_with_ambient_background() -> (SceneGraph, String) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let zone_name = "ambient-background".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "Ambient background zone — full display, behind all content"
                    .to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.0,
                    y_pct: 0.0,
                    width_pct: 1.0,
                    height_pct: 1.0,
                },
                accepted_media_types: vec![ZoneMediaType::SolidColor, ZoneMediaType::StaticImage],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::Replace,
                max_publishers: 1,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Background,
            },
        );
        (scene, zone_name)
    }

    /// Requirement: MCP StaticImage Content Type Dispatch / User-Test Scenarios
    /// Scenario: "Solid color via MCP to ambient-background"
    ///
    /// Send `publish_to_zone` with `{"type": "solid_color", "r": 0.05, "g": 0.05, "b": 0.2, "a": 1.0}`.
    /// Verify the zone's active publication contains `ZoneContent::SolidColor(Rgba{0.05, 0.05, 0.2, 1.0})`.
    ///
    /// Covers: spec.md §"Solid color via MCP to ambient-background",
    ///         tasks.md §4.1 (ambient-background solid color user-test scenario).
    #[test]
    fn test_mcp_ambient_background_solid_color() {
        let (mut scene, zone) = scene_with_ambient_background();

        let result = handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "content": {"type": "solid_color", "r": 0.05, "g": 0.05, "b": 0.2, "a": 1.0}
            }),
            &mut scene,
        )
        .unwrap();

        assert_eq!(result.zone_name, zone);

        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            publishes.len(),
            1,
            "ambient-background Replace policy must yield exactly 1 active publication"
        );

        match &publishes[0].content {
            tze_hud_scene::types::ZoneContent::SolidColor(rgba) => {
                assert!(
                    (rgba.r - 0.05f32).abs() < 1e-4,
                    "r must be 0.05, got {}",
                    rgba.r
                );
                assert!(
                    (rgba.g - 0.05f32).abs() < 1e-4,
                    "g must be 0.05, got {}",
                    rgba.g
                );
                assert!(
                    (rgba.b - 0.2f32).abs() < 1e-4,
                    "b must be 0.2, got {}",
                    rgba.b
                );
                assert!(
                    (rgba.a - 1.0f32).abs() < 1e-4,
                    "a must be 1.0, got {}",
                    rgba.a
                );
            }
            other => panic!("expected ZoneContent::SolidColor, got: {other:?}"),
        }
    }

    /// Requirement: MCP StaticImage Content Type Dispatch
    /// Scenario: "Publish static image via MCP"
    ///
    /// Send `publish_to_zone` with `{"type": "static_image", "resource_id": "abc123def456..."}`.
    /// Verify the zone's active publication contains `ZoneContent::StaticImage(ResourceId)`.
    ///
    /// Covers: spec.md §"Publish static image via MCP",
    ///         tasks.md §1.4, §4.3.
    #[test]
    fn test_mcp_ambient_background_static_image() {
        let (mut scene, zone) = scene_with_ambient_background();

        // A valid 64-char hex string (blake3 hash of b"test").
        let resource_id_hex = "4878ca0425c739fa427f7eda20fe845f6b2f46ba5fe5ac7d6b85add8db6bb08f";

        let result = handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "content": {"type": "static_image", "resource_id": resource_id_hex}
            }),
            &mut scene,
        )
        .unwrap();

        assert_eq!(result.zone_name, zone);

        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            publishes.len(),
            1,
            "ambient-background Replace policy must yield exactly 1 active publication after static_image publish"
        );

        assert!(
            matches!(
                &publishes[0].content,
                tze_hud_scene::types::ZoneContent::StaticImage(_)
            ),
            "publish with static_image content must produce ZoneContent::StaticImage, got: {:?}",
            &publishes[0].content
        );
    }

    /// Requirement: MCP StaticImage Content Type Dispatch
    /// Scenario: "Missing resource_id returns error"
    ///
    /// Send `publish_to_zone` with `{"type": "static_image"}` (no `resource_id`).
    /// Assert `invalid_params` error is returned.
    ///
    /// Covers: spec.md §"Missing resource_id returns error",
    ///         tasks.md §1.5.
    #[test]
    fn test_mcp_ambient_background_static_image_missing_resource_id() {
        let (mut scene, zone) = scene_with_ambient_background();

        let err = handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "content": {"type": "static_image"}
            }),
            &mut scene,
        )
        .unwrap_err();

        assert!(
            matches!(&err, McpError::InvalidParams(msg) if msg.contains("resource_id")),
            "missing resource_id must return InvalidParams mentioning resource_id, got: {err:?}"
        );
    }

    /// Requirement: Ambient Background User-Test Scenarios
    /// Scenario: "Agent replaces background with warm amber" / Rapid replacement
    ///
    /// Publish dark blue then warm amber via MCP. Verify only amber is in
    /// active publications (latest-wins via Replace policy) and publication
    /// count is exactly 1.
    ///
    /// Also covers the rapid-replacement stress scenario (tasks.md §4.2):
    /// publish 10 different colors in sequence, assert exactly 1 active publication.
    ///
    /// Covers: spec.md §"Agent replaces background with warm amber",
    ///         spec.md §"Rapid replacement stress test",
    ///         tasks.md §4.1, §4.2.
    #[test]
    fn test_mcp_ambient_background_replacement_via_mcp() {
        let (mut scene, zone) = scene_with_ambient_background();

        // Phase 1: Publish 10 different colors in rapid succession to exercise
        // rapid-replacement stress scenario (tasks.md §4.2).
        let colors: &[(&str, f64, f64, f64)] = &[
            ("red", 1.0, 0.0, 0.0),
            ("green", 0.0, 1.0, 0.0),
            ("blue", 0.0, 0.0, 1.0),
            ("yellow", 1.0, 1.0, 0.0),
            ("magenta", 1.0, 0.0, 1.0),
            ("cyan", 0.0, 1.0, 1.0),
            ("gray", 0.5, 0.5, 0.5),
            ("orange", 1.0, 0.5, 0.0),
            ("purple", 0.5, 0.0, 0.5),
            // 10th: dark blue — initial anchor for the two-step replacement test.
            ("dark-blue", 0.05, 0.05, 0.2),
        ];

        for (label, r, g, b) in colors {
            handle_publish_to_zone(
                json!({
                    "zone_name": zone,
                    "content": {"type": "solid_color", "r": r, "g": g, "b": b, "a": 1.0}
                }),
                &mut scene,
            )
            .unwrap_or_else(|e| panic!("publish {label} must succeed: {e:?}"));
        }

        // After 10 rapid publishes the Replace policy must keep exactly 1 record.
        let pub_count_after_rapid = scene
            .zone_registry
            .active_publishes
            .get(&zone)
            .map(|v| v.len())
            .unwrap_or(0);
        assert_eq!(
            pub_count_after_rapid, 1,
            "ambient-background active publication count MUST be exactly 1 after 10 rapid Replace \
             publishes (rapid-replacement stress scenario); got {pub_count_after_rapid}"
        );

        // Phase 2: Replace dark blue with warm amber — the canonical "agent replaces
        // background" scenario (spec.md §"Agent replaces background with warm amber").
        handle_publish_to_zone(
            json!({
                "zone_name": zone,
                "content": {"type": "solid_color", "r": 0.9, "g": 0.6, "b": 0.2, "a": 1.0}
            }),
            &mut scene,
        )
        .unwrap();

        let publishes = scene.zone_registry.active_publishes.get(&zone).unwrap();
        assert_eq!(
            publishes.len(),
            1,
            "active publication count must still be 1 after warm-amber replace"
        );

        // Verify the active publication is warm amber, not dark blue.
        match &publishes[0].content {
            tze_hud_scene::types::ZoneContent::SolidColor(rgba) => {
                assert!(
                    (rgba.r - 0.9f32).abs() < 1e-3,
                    "r must be ~0.9 (warm amber), got {}",
                    rgba.r
                );
                assert!(
                    (rgba.g - 0.6f32).abs() < 1e-3,
                    "g must be ~0.6 (warm amber), got {}",
                    rgba.g
                );
                assert!(
                    (rgba.b - 0.2f32).abs() < 1e-3,
                    "b must be ~0.2 (warm amber), got {}",
                    rgba.b
                );
            }
            other => panic!(
                "active publication must be warm amber SolidColor after replacement, got: {other:?}"
            ),
        }
    }

    /// Requirement: Ambient Background User-Test Scenarios
    /// Scenario: "Agent sets solid color background" — zone registry alignment check.
    ///
    /// Verify that `ZoneRegistry::with_defaults()` contains an `ambient-background`
    /// zone with the expected spec properties:
    /// - ContentionPolicy::Replace, max_publishers=1
    /// - LayerAttachment::Background
    /// - Full-screen geometry (Relative {0,0,1,1})
    /// - accepted_media_types includes SolidColor and StaticImage
    /// - auto_clear_ms = None (persistent until replaced)
    ///
    /// Covers: tasks.md §5.1 (zone registry alignment),
    ///         spec.md §"Ambient Background Zone Visual Contract".
    #[test]
    fn test_ambient_background_zone_registry_alignment() {
        use tze_hud_scene::types::ZoneRegistry;
        let registry = ZoneRegistry::with_defaults();
        let zone = registry
            .zones
            .get("ambient-background")
            .expect("ambient-background zone must exist in ZoneRegistry::with_defaults()");

        // Contention policy: Replace with max_publishers=1 (latest-wins).
        assert!(
            matches!(zone.contention_policy, ContentionPolicy::Replace),
            "ambient-background must use ContentionPolicy::Replace, got: {:?}",
            zone.contention_policy
        );
        assert_eq!(
            zone.max_publishers, 1,
            "ambient-background must have max_publishers=1"
        );

        // Layer: Background (behind all content-layer tiles).
        assert!(
            matches!(zone.layer_attachment, LayerAttachment::Background),
            "ambient-background must use LayerAttachment::Background, got: {:?}",
            zone.layer_attachment
        );

        // Geometry: full-screen (Relative 0,0,1,1).
        assert!(
            matches!(
                zone.geometry_policy,
                GeometryPolicy::Relative {
                    x_pct,
                    y_pct,
                    width_pct,
                    height_pct,
                } if x_pct == 0.0 && y_pct == 0.0 && width_pct == 1.0 && height_pct == 1.0
            ),
            "ambient-background must have full-screen Relative geometry {{0,0,1,1}}, got: {:?}",
            zone.geometry_policy
        );

        // Accepted media types: SolidColor and StaticImage (v1 mandatory).
        assert!(
            zone.accepted_media_types
                .contains(&ZoneMediaType::SolidColor),
            "ambient-background must accept ZoneMediaType::SolidColor"
        );
        assert!(
            zone.accepted_media_types
                .contains(&ZoneMediaType::StaticImage),
            "ambient-background must accept ZoneMediaType::StaticImage"
        );

        // TTL: no auto-clear (persistent until replaced).
        assert_eq!(
            zone.auto_clear_ms, None,
            "ambient-background must have auto_clear_ms=None (persistent until replaced)"
        );
    }

    #[test]
    fn test_list_elements_returns_all_types_and_supports_filters() {
        let (mut scene, tab_id) = scene_with_widget();

        // Add one zone.
        let zone_name = "list-elements-zone".to_string();
        scene.zone_registry.zones.insert(
            zone_name.clone(),
            ZoneDefinition {
                id: SceneId::new(),
                name: zone_name.clone(),
                description: "ListElements test zone".to_string(),
                geometry_policy: GeometryPolicy::Relative {
                    x_pct: 0.1,
                    y_pct: 0.1,
                    width_pct: 0.6,
                    height_pct: 0.2,
                },
                accepted_media_types: vec![ZoneMediaType::StreamText],
                rendering_policy: RenderingPolicy::default(),
                contention_policy: ContentionPolicy::LatestWins,
                max_publishers: 2,
                transport_constraint: None,
                auto_clear_ms: None,
                ephemeral: false,
                layer_attachment: LayerAttachment::Content,
            },
        );

        // Add one tile.
        let lease_id = scene.grant_lease(
            "agent.list-elements",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent.list-elements",
                lease_id,
                Rect::new(192.0, 108.0, 384.0, 216.0),
                1,
            )
            .expect("create tile");

        // Publish once to zone and widget so last_published_at fields are populated.
        handle_publish_to_zone(
            json!({
                "zone_name": zone_name,
                "content": "zone payload",
                "namespace": "agent.list-elements"
            }),
            &mut scene,
        )
        .expect("publish zone");
        handle_publish_to_widget(
            json!({
                "widget_name": "gauge",
                "params": {"level": 0.42},
                "namespace": "agent.list-elements"
            }),
            &mut scene,
            &["publish_widget:gauge".to_string()],
        )
        .expect("publish widget");

        let all = handle_list_elements(json!({}), &scene).expect("list all elements");
        assert!(
            all.elements.iter().any(|e| e.element_type == "tile"
                && e.element_id == tile_id.to_string()
                && e.namespace == "agent.list-elements"),
            "expected tile entry in list_elements result"
        );
        assert!(
            all.elements.iter().any(|e| e.element_type == "zone"
                && e.namespace == zone_name
                && e.last_published_at_ms > 0),
            "expected zone entry with last_published_at_ms > 0"
        );
        assert!(
            all.elements.iter().any(|e| e.element_type == "widget"
                && e.namespace == "gauge"
                && e.last_published_at_ms > 0),
            "expected widget entry with last_published_at_ms > 0"
        );

        let tile_only = handle_list_elements(json!({"element_type": "tile"}), &scene)
            .expect("list tile elements");
        assert_eq!(tile_only.count, 1);
        assert_eq!(tile_only.elements[0].element_type, "tile");

        let namespace_filtered = handle_list_elements(
            json!({"namespace_filter": "agent.list-elements", "element_type": "tile"}),
            &scene,
        )
        .expect("list namespace-filtered elements");
        assert_eq!(namespace_filtered.count, 1);
        assert_eq!(
            namespace_filtered.elements[0].namespace,
            "agent.list-elements"
        );
    }

    #[test]
    fn test_publish_to_element_tile_by_id_sets_markdown_content() {
        let (mut scene, tab_id) = scene_with_tab();
        let lease_id = scene.grant_lease(
            "agent.publish-element",
            60_000,
            vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
        );
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent.publish-element",
                lease_id,
                Rect::new(0.0, 0.0, 320.0, 120.0),
                1,
            )
            .expect("create tile");

        let result = handle_publish_to_element(
            json!({
                "element_id": tile_id.to_string(),
                "content": "tile payload"
            }),
            &mut scene,
            &[],
        )
        .expect("publish to tile by element id");

        assert_eq!(result.element_type, "tile");
        let tile = scene.tiles.get(&tile_id).expect("tile should exist");
        let root_id = tile.root_node.expect("tile root should be set");
        let root = scene.nodes.get(&root_id).expect("root node should exist");
        match &root.data {
            NodeData::TextMarkdown(markdown) => assert_eq!(markdown.content, "tile payload"),
            other => panic!("expected markdown node, got {other:?}"),
        }
    }

    #[test]
    fn test_publish_to_element_zone_by_id_routes_to_zone_publish() {
        let (mut scene, _, zone_name) = scene_with_zone();
        let zone_id = scene
            .zone_registry
            .zones
            .get(&zone_name)
            .expect("zone exists")
            .id;

        let result = handle_publish_to_element(
            json!({
                "element_id": zone_id.to_string(),
                "content": "zone payload",
                "namespace": "agent.zone"
            }),
            &mut scene,
            &[],
        )
        .expect("publish to zone by element id");

        assert_eq!(result.element_type, "zone");
        let publishes = scene
            .zone_registry
            .active_publishes
            .get(&zone_name)
            .expect("zone publish records should exist");
        assert_eq!(publishes.len(), 1);
        assert!(
            matches!(&publishes[0].content, ZoneContent::StreamText(s) if s == "zone payload"),
            "zone content must be routed through publish_to_zone path"
        );
    }

    #[test]
    fn test_publish_to_element_widget_by_id_routes_to_widget_publish() {
        let (mut scene, _) = scene_with_widget();
        let widget_id = scene
            .widget_registry
            .instances
            .get("gauge")
            .expect("widget instance should exist")
            .id;

        let result = handle_publish_to_element(
            json!({
                "element_id": widget_id.to_string(),
                "content": {"level": 0.73},
                "namespace": "agent.widget"
            }),
            &mut scene,
            &["publish_widget:gauge".to_string()],
        )
        .expect("publish to widget by element id");

        assert_eq!(result.element_type, "widget");
        let publishes = scene.widget_registry.active_for_widget("gauge");
        assert_eq!(publishes.len(), 1);
        assert_eq!(publishes[0].publisher_namespace, "agent.widget");
    }

    #[test]
    fn test_publish_to_element_unknown_id_returns_element_not_found() {
        let (mut scene, _) = scene_with_tab();
        let err = handle_publish_to_element(
            json!({
                "element_id": SceneId::new().to_string(),
                "content": "payload"
            }),
            &mut scene,
            &[],
        )
        .expect_err("unknown id should fail");
        assert!(
            matches!(err, McpError::SceneError(ref msg) if msg.contains("ELEMENT_NOT_FOUND")),
            "expected ELEMENT_NOT_FOUND scene error, got {err:?}"
        );
    }

    // ── portal_projection_publish params (hud-m7w3g) ──────────────────────────

    #[test]
    fn portal_publish_params_default_classification_fields_to_none() {
        let p: PortalProjectionPublishParams = parse_params(json!({
            "projection_id": "p1",
            "owner_token": "t1",
            "output_text": "hello"
        }))
        .expect("minimal publish params must parse");
        assert_eq!(p.logical_unit_id, None);
        assert_eq!(p.output_kind, None);
        assert_eq!(p.content_classification, None);
        assert_eq!(p.coalesce_key, None);
        assert_eq!(p.expects_reply, None);
    }

    #[test]
    fn portal_publish_params_carry_classification_and_coalesce_key() {
        let p: PortalProjectionPublishParams = parse_params(json!({
            "projection_id": "p1",
            "owner_token": "t1",
            "output_text": "hello",
            "output_kind": "status",
            "content_classification": "public",
            "coalesce_key": "ck-1"
        }))
        .expect("full publish params must parse");
        assert_eq!(p.output_kind.as_deref(), Some("status"));
        assert_eq!(p.content_classification.as_deref(), Some("public"));
        assert_eq!(p.coalesce_key.as_deref(), Some("ck-1"));
    }

    /// hud-jip0k: `expects_reply` is optional and absent-by-default, matching
    /// the backward-compat requirement — an omitted flag must parse identically
    /// to a caller that predates the field.
    #[test]
    fn portal_publish_params_expects_reply_defaults_to_none() {
        let p: PortalProjectionPublishParams = parse_params(json!({
            "projection_id": "p1",
            "owner_token": "t1",
            "output_text": "hello"
        }))
        .expect("minimal publish params must parse");
        assert_eq!(p.expects_reply, None);
    }

    #[test]
    fn portal_publish_params_carry_expects_reply_true() {
        let p: PortalProjectionPublishParams = parse_params(json!({
            "projection_id": "p1",
            "owner_token": "t1",
            "output_text": "which option do you prefer?",
            "expects_reply": true
        }))
        .expect("publish params with expects_reply must parse");
        assert_eq!(p.expects_reply, Some(true));
    }

    #[tokio::test]
    async fn portal_list_rejects_caller_supplied_scope_or_credentials() {
        for params in [
            json!({ "projection_id": "another-callers-projection" }),
            json!({ "owner_token": "not-an-input-to-list" }),
        ] {
            let error = handle_portal_projection_list(params, None)
                .await
                .expect_err("list must reject caller-supplied scope or credentials");
            assert!(
                matches!(error, McpError::InvalidParams(_)),
                "list must reject unexpected arguments before checking runtime wiring: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn portal_list_forwards_op_and_returns_content_free_summaries() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("op must be sent") {
                crate::portal_op::PortalOp::List { reply } => {
                    reply
                        .send(Ok(crate::portal_op::ProjectionListBatch {
                            projections: vec![crate::portal_op::ProjectionListEntry {
                                projection_id: "alpha".to_string(),
                                display_name: "Alpha session".to_string(),
                                lifecycle_state: "active".to_string(),
                                unread_output_count: 2,
                                pending_input_count: 1,
                            }],
                        }))
                        .expect("reply must send");
                }
                other => panic!("unexpected op: {other:?}"),
            }
        });

        let result = handle_portal_projection_list(json!({}), Some(&tx))
            .await
            .expect("caller-scoped list must succeed");
        responder.await.expect("responder task must finish");

        assert_eq!(result.projections.len(), 1);
        assert_eq!(result.projections[0].projection_id, "alpha");
        let payload = serde_json::to_value(result).expect("list result must serialize");
        assert_eq!(
            payload,
            json!({
                "projections": [{
                    "projection_id": "alpha",
                    "display_name": "Alpha session",
                    "lifecycle_state": "active",
                    "unread_output_count": 2,
                    "pending_input_count": 1,
                }]
            }),
            "MCP list must expose only the bounded summary contract"
        );
    }

    // ── portal_projection get_pending_input / acknowledge / detach (hud-bq0gl.1) ─

    #[tokio::test]
    async fn portal_get_pending_input_rejects_empty_token() {
        let err = handle_portal_projection_get_pending_input(
            json!({ "projection_id": "p1", "owner_token": "" }),
            None,
        )
        .await
        .expect_err("empty owner_token must be rejected");
        assert!(matches!(err, McpError::InvalidParams(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn portal_get_pending_input_errors_when_authority_unwired() {
        let err = handle_portal_projection_get_pending_input(
            json!({ "projection_id": "p1", "owner_token": "t1" }),
            None,
        )
        .await
        .expect_err("unwired authority must error");
        assert!(matches!(err, McpError::Internal(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn portal_get_pending_input_forwards_op_and_returns_batch() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        // Authority stand-in: reply to the GetPendingInput op with one item.
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("op must be sent") {
                crate::portal_op::PortalOp::GetPendingInput {
                    projection_id,
                    owner_token,
                    max_items,
                    reply,
                    ..
                } => {
                    assert_eq!(projection_id, "p1");
                    assert_eq!(owner_token, "t1");
                    assert_eq!(max_items, Some(5));
                    reply
                        .send(Ok(crate::portal_op::PendingInputBatch {
                            items: vec![crate::portal_op::PendingInputEntry {
                                input_id: "i1".to_string(),
                                projection_id: "p1".to_string(),
                                submission_text: "hi".to_string(),
                                submitted_at_wall_us: 1,
                                expires_at_wall_us: 2,
                                delivery_state: "delivered".to_string(),
                                content_classification: "household".to_string(),
                            }],
                            remaining_count: 3,
                            remaining_bytes: 42,
                        }))
                        .expect("reply must send");
                }
                other => panic!("unexpected op: {other:?}"),
            }
        });
        let result = handle_portal_projection_get_pending_input(
            json!({ "projection_id": "p1", "owner_token": "t1", "max_items": 5 }),
            Some(&tx),
        )
        .await
        .expect("poll must succeed");
        responder.await.expect("responder task must finish");
        assert!(result.accepted);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].input_id, "i1");
        assert_eq!(result.remaining_count, 3);
        assert_eq!(result.remaining_bytes, 42);
    }

    /// Long-poll (hud-p4ufx): with `wait_ms` set, a poll that immediately finds
    /// an item returns at once — it must NOT keep polling for the full wait.
    #[tokio::test]
    async fn portal_get_pending_input_long_poll_returns_immediately_when_items_present() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let polls = Arc::new(AtomicUsize::new(0));
        let polls_seen = polls.clone();
        let responder = tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                if let crate::portal_op::PortalOp::GetPendingInput { reply, .. } = op {
                    polls_seen.fetch_add(1, Ordering::SeqCst);
                    reply
                        .send(Ok(crate::portal_op::PendingInputBatch {
                            items: vec![crate::portal_op::PendingInputEntry {
                                input_id: "i1".to_string(),
                                projection_id: "p1".to_string(),
                                submission_text: "hi".to_string(),
                                submitted_at_wall_us: 1,
                                expires_at_wall_us: 2,
                                delivery_state: "delivered".to_string(),
                                content_classification: "household".to_string(),
                            }],
                            remaining_count: 0,
                            remaining_bytes: 0,
                        }))
                        .expect("reply must send");
                }
            }
        });
        let result = handle_portal_projection_get_pending_input(
            json!({ "projection_id": "p1", "owner_token": "t1", "wait_ms": 5000 }),
            Some(&tx),
        )
        .await
        .expect("poll must succeed");
        assert!(result.accepted);
        assert_eq!(result.items.len(), 1);
        assert_eq!(
            polls.load(Ordering::SeqCst),
            1,
            "an immediately-available item must end the long-poll after a single poll"
        );
        drop(tx);
        responder.await.expect("responder task must finish");
    }

    /// Long-poll (hud-p4ufx): with no input available, the call re-polls until
    /// the wait elapses and returns an empty batch — without busy-spinning. We
    /// assert >=2 polls occurred (proving the bounded wait loop ran) rather than
    /// asserting wall-clock time, which would be flaky.
    #[tokio::test]
    async fn portal_get_pending_input_long_poll_repolls_then_returns_empty() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let polls = Arc::new(AtomicUsize::new(0));
        let polls_seen = polls.clone();
        let responder = tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                if let crate::portal_op::PortalOp::GetPendingInput { reply, .. } = op {
                    polls_seen.fetch_add(1, Ordering::SeqCst);
                    reply
                        .send(Ok(crate::portal_op::PendingInputBatch {
                            items: vec![],
                            remaining_count: 0,
                            remaining_bytes: 0,
                        }))
                        .expect("reply must send");
                }
            }
        });
        let result = handle_portal_projection_get_pending_input(
            json!({ "projection_id": "p1", "owner_token": "t1", "wait_ms": 400 }),
            Some(&tx),
        )
        .await
        .expect("poll must succeed");
        assert!(result.accepted);
        assert!(
            result.items.is_empty(),
            "long-poll with no input must return an empty batch, not block forever"
        );
        assert!(
            polls.load(Ordering::SeqCst) >= 2,
            "a 400ms wait at a 150ms poll interval must re-poll at least twice (got {})",
            polls.load(Ordering::SeqCst)
        );
        drop(tx);
        responder.await.expect("responder task must finish");
    }

    /// Long-poll (hud-p4ufx review follow-up): when input is pending but does not
    /// fit the caller's budget, the authority returns an empty batch with
    /// `remaining_count > 0`. That is backpressure, not "no input yet" — the call
    /// must return immediately (after a single poll) so the caller can raise its
    /// cap, rather than stalling for the full wait re-polling an over-budget item
    /// that can never fit.
    #[tokio::test]
    async fn portal_get_pending_input_long_poll_returns_immediately_on_budget_backpressure() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let polls = Arc::new(AtomicUsize::new(0));
        let polls_seen = polls.clone();
        let responder = tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                if let crate::portal_op::PortalOp::GetPendingInput { reply, .. } = op {
                    polls_seen.fetch_add(1, Ordering::SeqCst);
                    reply
                        .send(Ok(crate::portal_op::PendingInputBatch {
                            items: vec![],
                            remaining_count: 1,
                            remaining_bytes: 4096,
                        }))
                        .expect("reply must send");
                }
            }
        });
        let result = handle_portal_projection_get_pending_input(
            json!({ "projection_id": "p1", "owner_token": "t1", "wait_ms": 5000, "max_bytes": 8 }),
            Some(&tx),
        )
        .await
        .expect("poll must succeed");
        assert!(result.accepted);
        assert!(
            result.items.is_empty(),
            "an over-budget item yields an empty batch"
        );
        assert_eq!(
            result.remaining_count, 1,
            "backpressure must surface the pending-but-unfit item count"
        );
        assert_eq!(
            polls.load(Ordering::SeqCst),
            1,
            "budget backpressure must end the long-poll after a single poll, not stall for the full wait"
        );
        drop(tx);
        responder.await.expect("responder task must finish");
    }

    /// hud-s8a62 acceptance: a known authority rejection (TOKEN_EXPIRED) must
    /// reach the MCP layer as the stable `PROJECTION_*` code in
    /// `error.data.error_code` — not flatten into an opaque `-32603` with no
    /// actionable code. The skill instructs the LLM to branch on these codes,
    /// so the structured code must survive the reply-channel hop.
    #[tokio::test]
    async fn portal_publish_rejection_surfaces_stable_projection_code() {
        use tze_hud_projection::ProjectionErrorCode;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        // Authority stand-in: reject the publish with a TOKEN_EXPIRED code.
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("op must be sent") {
                crate::portal_op::PortalOp::PublishOutput { reply, .. } => {
                    reply
                        .send(Err(crate::portal_op::PortalOpRejection::new(
                            ProjectionErrorCode::ProjectionTokenExpired,
                            "owner token expired",
                        )))
                        .expect("reply must send");
                }
                other => panic!("unexpected op: {other:?}"),
            }
        });

        let err = handle_portal_projection_publish(
            json!({ "projection_id": "p1", "owner_token": "t1", "output_text": "hi" }),
            Some(&tx),
        )
        .await
        .expect_err("a TOKEN_EXPIRED rejection must surface as an error");
        responder.await.expect("responder task must finish");

        // The structured McpError carries the typed code, not a flattened string.
        match &err {
            McpError::ProjectionRejected { error_code, .. } => {
                assert_eq!(*error_code, ProjectionErrorCode::ProjectionTokenExpired);
            }
            other => panic!("expected ProjectionRejected, got {other:?}"),
        }

        // On the wire: the rejection has its dedicated application code plus
        // deterministic recovery guidance, rather than JSON-RPC Internal.
        let wire: crate::error::JsonRpcError = err.into();
        assert_eq!(wire.code, -32103);
        let data = wire.data.expect("rejection must carry structured data");
        assert_eq!(data["error_code"], "PROJECTION_TOKEN_EXPIRED");
        assert_eq!(
            data["hint"]["recovery_operation"],
            "portal_projection_attach"
        );
    }

    #[tokio::test]
    async fn portal_publish_status_rejects_empty_fields() {
        for params in [
            json!({ "projection_id": "", "owner_token": "t", "lifecycle_state": "active" }),
            json!({ "projection_id": "p", "owner_token": "", "lifecycle_state": "active" }),
            json!({ "projection_id": "p", "owner_token": "t", "lifecycle_state": "" }),
        ] {
            let err = handle_portal_projection_publish_status(params, None)
                .await
                .expect_err("empty required field must be rejected");
            assert!(matches!(err, McpError::InvalidParams(_)), "got {err:?}");
        }
    }

    /// hud-y8h3m acceptance (4): a `publish_status` call must drive through the
    /// MCP method to the authority over the portal-op channel, forwarding the
    /// lifecycle_state + status_text, and the applied state must round-trip back.
    #[tokio::test]
    async fn portal_publish_status_forwards_op_and_echoes_state() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        // Authority stand-in: assert the forwarded fields, echo the applied state.
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("op must be sent") {
                crate::portal_op::PortalOp::PublishStatus {
                    projection_id,
                    owner_token,
                    lifecycle_state,
                    status_text,
                    reply,
                } => {
                    assert_eq!(projection_id, "p1");
                    assert_eq!(owner_token, "t1");
                    assert_eq!(lifecycle_state, "degraded");
                    assert_eq!(status_text.as_deref(), Some("blocked on input"));
                    // Echo the applied lifecycle state, as the real driver does.
                    reply.send(Ok(lifecycle_state)).expect("reply must send");
                }
                other => panic!("unexpected op: {other:?}"),
            }
        });

        let result = handle_portal_projection_publish_status(
            json!({
                "projection_id": "p1",
                "owner_token": "t1",
                "lifecycle_state": "degraded",
                "status_text": "blocked on input"
            }),
            Some(&tx),
        )
        .await
        .expect("publish_status must succeed");
        responder.await.expect("responder task must finish");
        assert!(result.accepted);
        assert_eq!(
            result.lifecycle_state, "degraded",
            "the applied lifecycle state must round-trip back to the MCP caller"
        );
    }

    /// A `publish_status` authority rejection must surface as a structured
    /// `ProjectionRejected` carrying the stable `PROJECTION_*` code (hud-y8h3m
    /// reuses the hud-s8a62 typed-rejection channel).
    #[tokio::test]
    async fn portal_publish_status_rejection_surfaces_stable_projection_code() {
        use tze_hud_projection::ProjectionErrorCode;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("op must be sent") {
                crate::portal_op::PortalOp::PublishStatus { reply, .. } => {
                    reply
                        .send(Err(crate::portal_op::PortalOpRejection::new(
                            ProjectionErrorCode::ProjectionTokenExpired,
                            "owner token expired",
                        )))
                        .expect("reply must send");
                }
                other => panic!("unexpected op: {other:?}"),
            }
        });

        let err = handle_portal_projection_publish_status(
            json!({ "projection_id": "p1", "owner_token": "t1", "lifecycle_state": "active" }),
            Some(&tx),
        )
        .await
        .expect_err("a TOKEN_EXPIRED rejection must surface as an error");
        responder.await.expect("responder task must finish");

        match &err {
            McpError::ProjectionRejected { error_code, .. } => {
                assert_eq!(*error_code, ProjectionErrorCode::ProjectionTokenExpired);
            }
            other => panic!("expected ProjectionRejected, got {other:?}"),
        }
        let wire: crate::error::JsonRpcError = err.into();
        let data = wire.data.expect("rejection must carry structured data");
        assert_eq!(data["error_code"], "PROJECTION_TOKEN_EXPIRED");
    }

    #[tokio::test]
    async fn portal_acknowledge_input_rejects_empty_fields() {
        for params in [
            json!({ "projection_id": "", "owner_token": "t", "input_id": "i", "ack_state": "handled" }),
            json!({ "projection_id": "p", "owner_token": "", "input_id": "i", "ack_state": "handled" }),
            json!({ "projection_id": "p", "owner_token": "t", "input_id": "", "ack_state": "handled" }),
            json!({ "projection_id": "p", "owner_token": "t", "input_id": "i", "ack_state": "" }),
        ] {
            let err = handle_portal_projection_acknowledge_input(params, None)
                .await
                .expect_err("empty required field must be rejected");
            assert!(matches!(err, McpError::InvalidParams(_)), "got {err:?}");
        }
    }

    #[tokio::test]
    async fn portal_acknowledge_input_forwards_op() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("op must be sent") {
                crate::portal_op::PortalOp::AcknowledgeInput {
                    input_id,
                    ack_state,
                    reply,
                    ..
                } => {
                    assert_eq!(input_id, "i1");
                    assert_eq!(ack_state, "handled");
                    reply.send(Ok(())).expect("reply must send");
                }
                other => panic!("unexpected op: {other:?}"),
            }
        });
        let result = handle_portal_projection_acknowledge_input(
            json!({
                "projection_id": "p1",
                "owner_token": "t1",
                "input_id": "i1",
                "ack_state": "handled"
            }),
            Some(&tx),
        )
        .await
        .expect("ack must succeed");
        responder.await.expect("responder task must finish");
        assert!(result.accepted);
    }

    #[tokio::test]
    async fn portal_detach_rejects_empty_reason() {
        let err = handle_portal_projection_detach(
            json!({ "projection_id": "p1", "owner_token": "t1", "reason": "" }),
            None,
        )
        .await
        .expect_err("empty reason must be rejected");
        assert!(matches!(err, McpError::InvalidParams(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn portal_detach_forwards_op() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("op must be sent") {
                crate::portal_op::PortalOp::Detach {
                    projection_id,
                    reason,
                    reply,
                    ..
                } => {
                    assert_eq!(projection_id, "p1");
                    assert_eq!(reason, "bye");
                    reply.send(Ok(())).expect("reply must send");
                }
                other => panic!("unexpected op: {other:?}"),
            }
        });
        let result = handle_portal_projection_detach(
            json!({ "projection_id": "p1", "owner_token": "t1", "reason": "bye" }),
            Some(&tx),
        )
        .await
        .expect("detach must succeed");
        responder.await.expect("responder task must finish");
        assert!(result.accepted);
    }

    #[tokio::test]
    async fn portal_cleanup_rejects_missing_authority_fields() {
        for params in [
            json!({ "projection_id": "", "cleanup_authority": "owner", "owner_token": "t", "reason": "cleanup" }),
            json!({ "projection_id": "p", "cleanup_authority": "", "owner_token": "t", "reason": "cleanup" }),
            json!({ "projection_id": "p", "cleanup_authority": "owner", "owner_token": "", "reason": "cleanup" }),
            json!({ "projection_id": "p", "cleanup_authority": "operator", "operator_authority": "", "reason": "cleanup" }),
            json!({ "projection_id": "p", "cleanup_authority": "operator", "operator_authority": "op", "reason": "" }),
        ] {
            let err = handle_portal_projection_cleanup(params, None)
                .await
                .expect_err("empty required cleanup field must be rejected");
            assert!(matches!(err, McpError::InvalidParams(_)), "got {err:?}");
        }
    }

    #[tokio::test]
    async fn portal_cleanup_forwards_operator_op_without_owner_token() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("op must be sent") {
                crate::portal_op::PortalOp::Cleanup {
                    projection_id,
                    cleanup_authority,
                    owner_token,
                    operator_authority,
                    reason,
                    reply,
                } => {
                    assert_eq!(projection_id, "p1");
                    assert_eq!(cleanup_authority, "operator");
                    assert_eq!(owner_token, None);
                    assert_eq!(operator_authority.as_deref(), Some("operator-secret"));
                    assert_eq!(reason, "operator override");
                    reply.send(Ok(())).expect("reply must send");
                }
                other => panic!("unexpected op: {other:?}"),
            }
        });
        let result = handle_portal_projection_cleanup(
            json!({
                "projection_id": "p1",
                "cleanup_authority": "operator",
                "operator_authority": "operator-secret",
                "reason": "operator override"
            }),
            Some(&tx),
        )
        .await
        .expect("operator cleanup must succeed");
        responder.await.expect("responder task must finish");
        assert!(result.accepted);
    }
}
