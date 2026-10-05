//! Core types for the scene graph, following RFC 0001.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// ─── IDs ────────────────────────────────────────────────────────────────────

/// Scene object ID — UUIDv7 (time-ordered, 16 bytes).
///
/// # Wire format
/// Serialized as 16 raw bytes in little-endian UUID byte order (as returned by
/// [`Uuid::to_bytes_le`]). The all-zero value (`[0u8; 16]`) is the null/absent
/// sentinel per RFC 0001 §1.1.
///
/// # Invariants
/// - `size_of::<SceneId>() == 16`
/// - Lexicographic sort order == creation-time order (UUIDv7 property)
/// - `SceneId::null().is_null() == true`
/// - `SceneId::new().is_null() == false` (freshly-generated IDs are never null)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SceneId(Uuid);

impl SceneId {
    pub fn new() -> Self {
        SceneId(Uuid::now_v7())
    }

    /// Create from raw UUID (for testing / deserialization).
    pub fn from_uuid(uuid: Uuid) -> Self {
        SceneId(uuid)
    }

    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Null/zero ID used as the "absent" sentinel (RFC 0001 §1.1).
    ///
    /// When encoded as a protobuf `bytes` field, this value serializes to 16
    /// zero bytes. Note that in proto3 an unset `bytes` field defaults to an
    /// empty vector (length 0), not 16 zero bytes; callers must explicitly
    /// handle the empty-bytes case (e.g., `proto_to_scene_id` returns `None`
    /// for empty input) and decide whether to treat it as this sentinel.
    pub fn null() -> Self {
        SceneId(Uuid::nil())
    }

    /// Returns `true` if this is the null/absent sentinel (`[0u8; 16]`).
    pub fn is_null(&self) -> bool {
        self.0.is_nil()
    }

    /// Nil/zero ID used as "none" sentinel in protobuf.
    ///
    /// Alias for [`Self::null`]; prefer `null()`/`is_null()` in new code.
    #[inline]
    pub fn nil() -> Self {
        Self::null()
    }

    /// Returns `true` if this is the nil/zero sentinel.
    ///
    /// Alias for [`Self::is_null`]; prefer `is_null()` in new code.
    #[inline]
    pub fn is_nil(&self) -> bool {
        self.is_null()
    }

    /// Serialize to 16 bytes in little-endian UUID byte order.
    ///
    /// Used for protobuf `bytes` fields. The encoding is stable and matches
    /// the wire contract from RFC 0001 §4.1.
    pub fn to_bytes_le(&self) -> [u8; 16] {
        self.0.to_bytes_le()
    }

    /// Deserialize from 16 bytes in little-endian UUID byte order.
    ///
    /// Returns `None` if the slice is not exactly 16 bytes.
    pub fn from_bytes_le(bytes: &[u8]) -> Option<Self> {
        let arr: [u8; 16] = bytes.try_into().ok()?;
        Some(SceneId(Uuid::from_bytes_le(arr)))
    }
}

impl Default for SceneId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SceneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ─── ResourceId ──────────────────────────────────────────────────────────────

/// Content-addressed resource identity — 32-byte BLAKE3 hash.
///
/// Two agents uploading identical content MUST receive the same `ResourceId`;
/// the runtime stores the resource once (RFC 0001 §1.1).
///
/// # Wire format
/// Stored and transmitted as raw 32 bytes. Hex encoding is a display/debug
/// concern only and MUST NOT appear on the wire or in storage.
///
/// # Invariants
/// - `size_of::<ResourceId>() == 32`
/// - Equality is byte equality — no normalisation
/// - `ResourceId::of(bytes) == ResourceId::of(same_bytes)` always
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResourceId([u8; 32]);

impl ResourceId {
    /// Compute the `ResourceId` for a byte payload using BLAKE3.
    pub fn of(data: &[u8]) -> Self {
        let hash = blake3::hash(data);
        ResourceId(*hash.as_bytes())
    }

    /// Wrap a raw 32-byte array directly (for deserialization / testing).
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        ResourceId(bytes)
    }

    /// Return the raw 32-byte hash.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Try to construct from a byte slice.
    ///
    /// Returns `None` if the slice is not exactly 32 bytes.
    pub fn from_slice(slice: &[u8]) -> Option<Self> {
        let arr: [u8; 32] = slice.try_into().ok()?;
        Some(ResourceId(arr))
    }

    /// Return a lowercase hex string for display / logging only.
    ///
    /// MUST NOT be used on the wire or in storage.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Parse a 64-character hex string (case-insensitive) back into a `ResourceId`.
    ///
    /// Returns `None` if the string is not exactly 64 hex characters.
    ///
    /// Used when a non-wire, non-storage hex representation (for example,
    /// `NotificationPayload.icon` in local/UI-facing data) must be resolved
    /// back into a content-addressed `ResourceId` for GPU texture lookup.
    /// Hex MUST NOT be used as a wire or storage format for `ResourceId`.
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16)? as u8;
            let lo = (chunk[1] as char).to_digit(16)? as u8;
            bytes[i] = (hi << 4) | lo;
        }
        Some(ResourceId(bytes))
    }
}

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

// ─── Geometry ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains_point(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }

    /// Check if this rect is fully contained within `outer`.
    pub fn is_within(&self, outer: &Rect) -> bool {
        self.x >= outer.x
            && self.y >= outer.y
            && self.x + self.width <= outer.x + outer.width
            && self.y + self.height <= outer.y + outer.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const WHITE: Rgba = Rgba {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    };
    pub const BLACK: Rgba = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
    pub const TRANSPARENT: Rgba = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };

    pub fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// Convert to [f32; 4] for GPU upload.
    pub fn to_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }
}

/// Runtime-owned lifecycle-affordance accent for a portal tile.
///
/// Painted by the compositor as a thin token-colored bar along the tile's left
/// edge to signal the portal's lifecycle state (active / waiting-for-input /
/// blocked / degraded / detached). Stored as overlay state keyed by tile id
/// (see [`crate::graph::overlay`]) rather than as a scene node, so it survives
/// `SetTileRoot`/`PublishToTile` content republishes and is updated via the
/// coalescible StateStream `SetTileLifecycleAccent` mutation — never a
/// per-republish `AddNode` (hud-m48i0 / hud-mzk74).
///
/// Color and width are token-resolved by the producer (the portal adapter
/// reads `portal.lifecycle.*` design tokens); no literal visual value lives in
/// the compositor.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LifecycleAccent {
    /// Token-resolved accent color for the current lifecycle group.
    pub color: Rgba,
    /// Bar width in pixels (left edge, full tile height). Always > 0 when stored.
    pub width_px: f32,
}

// ─── Portal surface (RFC 0013 §7.2 promotion) ────────────────────────────────
//
// First-class text-stream portal surface schema. RFC 0013 §7.2 authorizes, once
// the promotion evidence gate passes (hud-qfyfg, PASSED 2026-07-05), promotion of
// the Phase-0 raw-tile pilot to "a first-class runtime portal surface or node
// type". These types are the scene-model half of that promotion: a governed
// descriptor that groups the portal's named parts, identity, and lifecycle/
// display state into one coalescible object instead of an ad-hoc six-tile
// assembly.
//
// The part set and its cross-map to the Phase-0 raw tiles is defined by the
// `text-portal` component type (component-shape-language delta, hud-2ey2w). This
// schema carries no transcript history and adds no transport: a portal surface is
// declared over an existing lease-governed tile via the Transactional
// `SetPortalSurface` mutation and its lifecycle/display state is patched via the
// coalescible StateStream `UpdatePortalSurfaceState` mutation. Per-part layout,
// styling, and rendering are owned by the renderer promotion beads (hud-s4lrw);
// this type deliberately holds only the declarative surface contract.

/// Peer class of the party on the far side of a portal's text stream
/// (RFC 0013 §2.1 "optional peer class"). Advisory metadata only — the runtime's
/// governance does not branch on it; it exists for identity presentation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortalPeerClass {
    /// Unspecified / unknown peer class.
    #[default]
    Unspecified,
    /// A resident LLM session driving the portal.
    ResidentLlm,
    /// A human operator monitoring/replying into the portal.
    Operator,
    /// A human peer in a chat-style collaboration.
    HumanPeer,
    /// A local adapter bridging an external text environment.
    Adapter,
}

/// Portal lifecycle state (RFC 0013 §3.2 / §6.2). Drives the coalescible
/// left-edge [`LifecycleAccent`] the compositor already paints; carried on the
/// surface descriptor so the promoted surface owns its own lifecycle rather than
/// inferring it from tile state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortalLifecycleState {
    /// Unspecified / not yet reported.
    #[default]
    Unspecified,
    /// Live and streaming.
    Active,
    /// Awaiting a viewer reply.
    WaitingForInput,
    /// Blocked (e.g. backpressured or gated).
    Blocked,
    /// Degraded but still present.
    Degraded,
    /// Owning session disconnected; in the orphan grace window.
    Detached,
}

/// Collapsed vs. expanded portal presentation (RFC 0013 §3.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortalDisplayState {
    /// Unspecified / not yet reported.
    #[default]
    Unspecified,
    /// Collapsed summary card (identity + status + unread affordance).
    Collapsed,
    /// Expanded transcript-focused surface with reply affordance.
    Expanded,
}

/// One of the eight named parts of the first-class portal surface, per the
/// `text-portal` component-shape-language part model (hud-2ey2w). Each variant
/// cross-maps to the Phase-0 raw-tile assembly so promotion preserves — rather
/// than redefines — the proven layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PortalPartKind {
    /// Surface backdrop, outer border, and footer/status chrome (Phase-0 `frame`).
    Frame,
    /// Title + subtitle band; also the move/drag handle (header band of `frame`).
    Header,
    /// Bounded draft text, caret, and selection (Phase-0 `input_scroll`).
    Composer,
    /// Markdown-rendered transcript window (Phase-0 `output_scroll`).
    Transcript,
    /// Vertical pane split and resize handle (frame-internal `divider`).
    Divider,
    /// Collapsed/minimized representation (Phase-0 `minimized_icon`).
    CollapsedCard,
    /// Full-bounds input/redaction backstop beneath the surface (Phase-0 `capture_backstop`).
    CaptureBackstop,
    /// Transient move/resize gesture capture; hosts the scroll indicator (Phase-0 `drag_shield`).
    GestureShield,
}

impl PortalPartKind {
    /// All eight portal parts, in canonical (declaration) order.
    pub const ALL: [PortalPartKind; 8] = [
        PortalPartKind::Frame,
        PortalPartKind::Header,
        PortalPartKind::Composer,
        PortalPartKind::Transcript,
        PortalPartKind::Divider,
        PortalPartKind::CollapsedCard,
        PortalPartKind::CaptureBackstop,
        PortalPartKind::GestureShield,
    ];

    /// Whether this part carries text (and therefore participates in
    /// `text-portal` readability enforcement). Geometry-only parts
    /// (`Divider`, `CaptureBackstop`, `GestureShield`) return `false`.
    pub fn is_text_bearing(self) -> bool {
        matches!(
            self,
            PortalPartKind::Frame // footer/status text
                | PortalPartKind::Header
                | PortalPartKind::Composer
                | PortalPartKind::Transcript
                | PortalPartKind::CollapsedCard
        )
    }
}

/// A single named part of a [`PortalSurface`].
///
/// Carries the part's kind, its surface-local geometry, and an optional
/// reference to the scene node that backs its content — "one scene node per
/// part" (the renderer promotion, hud-s4lrw). `node` is `None` for parts whose
/// appearance is purely derived (e.g. a geometry-only `Divider`) or not yet
/// materialized. The referenced node is an ordinary visual node
/// (`TextMarkdown`/`SolidColor`/`HitRegion`/`StaticImage`) within the surface's
/// host tile, so the Phase-0 raw-tile assembly is expressible verbatim: point
/// each part at its existing raw-tile content node.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PortalPart {
    /// Which named part this is.
    pub kind: PortalPartKind,
    /// Surface-local geometry (origin + extent relative to the host tile).
    pub bounds: Rect,
    /// Scene node backing this part's content, if materialized.
    pub node: Option<SceneId>,
}

/// Stable identity / status metadata for a portal surface (RFC 0013 §2.1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PortalIdentity {
    /// Stable adapter-assigned session id (max [`PORTAL_SESSION_ID_MAX_BYTES`]).
    pub session_id: String,
    /// Human-readable display name (max [`PORTAL_DISPLAY_NAME_MAX_BYTES`]).
    pub display_name: String,
    /// Optional peer class of the far side of the stream.
    pub peer_class: PortalPeerClass,
}

/// A first-class text-stream portal surface (RFC 0013 §7.2 promotion).
///
/// Groups the [eight named parts](PortalPartKind) under one governed,
/// coalescible descriptor together with the portal's [identity](PortalIdentity)
/// and [lifecycle](PortalLifecycleState)/[display](PortalDisplayState) state,
/// replacing the ad-hoc six-tile raw assembly of the Phase-0 pilot.
///
/// Stored as runtime overlay state keyed by the host **tile id** (mirroring
/// [`LifecycleAccent`]), so it survives `SetTileRoot`/`PublishToTile` transcript
/// republishes that replace the tile's node tree. The surface is **content-layer
/// and lease-governed** exactly as the raw-tile pilot is (RFC 0013 §6); this
/// schema grants no capability beyond what the §7.2 promotion already permits and
/// materializes **no transcript history** — parts reference the existing
/// bounded-viewport nodes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PortalSurface {
    /// Identity / status metadata.
    pub identity: PortalIdentity,
    /// Current lifecycle state.
    pub lifecycle: PortalLifecycleState,
    /// Current collapsed/expanded display state.
    pub display_state: PortalDisplayState,
    /// The surface's named parts (at most [`PORTAL_MAX_PARTS`], one per kind).
    pub parts: Vec<PortalPart>,
}

/// Maximum number of parts on a portal surface — one per [`PortalPartKind`].
pub const PORTAL_MAX_PARTS: usize = 8;
/// Maximum UTF-8 byte length of [`PortalIdentity::session_id`].
pub const PORTAL_SESSION_ID_MAX_BYTES: usize = 256;
/// Maximum UTF-8 byte length of [`PortalIdentity::display_name`].
pub const PORTAL_DISPLAY_NAME_MAX_BYTES: usize = 128;

impl PortalSurface {
    /// Validate the structural invariants of a portal surface descriptor:
    /// - at most [`PORTAL_MAX_PARTS`] parts,
    /// - no duplicate part kind,
    /// - identity strings within their byte budgets,
    /// - all part `bounds` finite and non-negative in extent.
    ///
    /// Returns a human-readable reason on the first violation. This is a pure
    /// structural check; tile ownership / node reachability are enforced by the
    /// graph apply path.
    pub fn validate_structure(&self) -> Result<(), String> {
        if self.identity.session_id.len() > PORTAL_SESSION_ID_MAX_BYTES {
            return Err(format!(
                "session_id exceeds {PORTAL_SESSION_ID_MAX_BYTES} bytes (got {})",
                self.identity.session_id.len()
            ));
        }
        if self.identity.display_name.len() > PORTAL_DISPLAY_NAME_MAX_BYTES {
            return Err(format!(
                "display_name exceeds {PORTAL_DISPLAY_NAME_MAX_BYTES} bytes (got {})",
                self.identity.display_name.len()
            ));
        }
        if self.parts.len() > PORTAL_MAX_PARTS {
            return Err(format!(
                "portal surface has {} parts, exceeds max {PORTAL_MAX_PARTS}",
                self.parts.len()
            ));
        }
        let mut seen = 0u16;
        for part in &self.parts {
            let bit = 1u16 << (part.kind as u16);
            if seen & bit != 0 {
                return Err(format!("duplicate portal part kind {:?}", part.kind));
            }
            seen |= bit;
            let b = part.bounds;
            if !(b.x.is_finite() && b.y.is_finite() && b.width.is_finite() && b.height.is_finite())
            {
                return Err(format!("part {:?} has non-finite bounds", part.kind));
            }
            // Negative width/height are invalid for visual layout and can cause
            // rendering anomalies in the compositor; reject them defensively,
            // mirroring the non-negative extent expected of tile/node bounds.
            if b.width < 0.0 || b.height < 0.0 {
                return Err(format!(
                    "part {:?} has negative bounds extent (width {}, height {})",
                    part.kind, b.width, b.height
                ));
            }
        }
        Ok(())
    }
}

// ─── Enums ──────────────────────────────────────────────────────────────────

/// How image content is fitted within the node's bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ImageFitMode {
    /// Scale uniformly so the entire image is visible; may leave letterbox bars.
    #[default]
    Contain,
    /// Scale uniformly to cover the entire bounds; may crop the image.
    Cover,
    /// Stretch non-uniformly to fill bounds exactly.
    Fill,
    /// Like Contain but never scale up; display at native size if smaller than bounds.
    ScaleDown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputMode {
    Passthrough,
    #[default]
    Capture,
    LocalOnly,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FontFamily {
    #[default]
    SystemSansSerif,
    SystemMonospace,
    SystemSerif,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextAlign {
    #[default]
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextOverflow {
    #[default]
    Clip,
    Ellipsis,
}

// ─── Scene Objects ──────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: SceneId,
    pub name: String,
    pub display_order: u32,
    pub created_at_ms: u64,
}

/// Per-agent resource envelope enforced by the budget enforcement ladder.
///
/// This is the single canonical resource budget type for the entire tze_hud
/// system. It covers both the scene-model dimensions (used in `Tile`/`Lease`
/// for persistence and wire serialization) and the enforcement dimensions
/// (used by `tze_hud_scene::lease::budget` and `tze_hud_runtime::budget` for
/// per-mutation admission checks).
///
/// # Defaults (spec §Requirement: Per-Agent Resource Envelope)
///
/// | Field               | Default   | Hard max |
/// |---------------------|-----------|----------|
/// | max_tiles           | 8         | 64       |
/// | max_texture_bytes   | 256 MiB   | 2 GiB    |
/// | max_update_rate_hz  | 30.0      | 120.0    |
/// | max_nodes_per_tile  | 32        | 64       |
/// | max_active_leases   | 8         | 64       |
/// | max_concurrent_streams | 0      | 0 (v1)   |
///
/// # Memory layout
///
/// The `Tile` struct embeds this type by value. Per RFC 0001 §8, `Tile` must
/// remain under 200 bytes. Additions to this struct that would push `Tile` past
/// that limit require a corresponding RFC amendment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceBudget {
    /// Maximum number of tiles this agent may hold simultaneously.
    /// Range [1, 64]. Default: 8.
    pub max_tiles: u32,
    /// Maximum texture memory across all tiles, in bytes.
    /// Default: 256 MiB (spec §Per-Agent Resource Envelope).
    pub max_texture_bytes: u64,
    /// Maximum scene mutation rate in Hz (sliding window over last 1 second).
    /// Default: 30.0 Hz.
    pub max_update_rate_hz: f32,
    /// Maximum nodes per individual tile.
    /// Range [1, 64]. Default: 32.
    pub max_nodes_per_tile: u32,
    /// Maximum concurrent active leases for this agent.
    ///
    /// Checked at lease-grant time and re-verified at mutation intake
    /// (defence-in-depth). Range [1, 64]. Default: 8.
    ///
    /// Serialized with a default of 8 so that scene snapshots serialized
    /// before this field was added deserialize correctly.
    #[serde(default = "ResourceBudget::default_max_active_leases")]
    pub max_active_leases: u8,
    /// Maximum concurrent media streams.  Always 0 in v1; media deferred.
    ///
    /// Serialized with a default of 0 so that existing scene snapshots
    /// deserialize correctly.
    #[serde(default)]
    pub max_concurrent_streams: u8,
}

impl ResourceBudget {
    const fn default_max_active_leases() -> u8 {
        8
    }
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            max_tiles: 8,
            max_texture_bytes: 256 * 1024 * 1024, // 256 MiB
            max_update_rate_hz: 30.0,
            max_nodes_per_tile: 32,
            max_active_leases: 8,
            max_concurrent_streams: 0,
        }
    }
}

/// Spatial constraints on a tile's dimensions, enforced at interactive resize time.
///
/// Stored on the `Lease` object (see `Lease::spatial_budget`).  Not embedded in
/// `Tile` — the `Tile` struct size is budgeted at < 200 bytes (RFC 0001 §8) and
/// spatial limits are not needed on the tile itself.
///
/// `0.0` for either dimension means unconstrained; the display boundary is the
/// hard outer clamp in all cases.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TileSpatialBudget {
    /// Maximum tile width the lease permits, in logical pixels.
    /// `0.0` = unconstrained (display boundary is the only clamp).
    pub max_tile_width_px: f32,
    /// Maximum tile height the lease permits, in logical pixels.
    /// `0.0` = unconstrained (display boundary is the only clamp).
    pub max_tile_height_px: f32,
}

/// A dimension in which an agent has violated its resource budget.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BudgetViolation {
    /// Agent holds more tiles than `max_tiles`.
    TileCountExceeded { current: u32, limit: u32 },
    /// Texture memory across all tiles exceeds `max_texture_bytes`.
    TextureMemoryExceeded {
        current_bytes: u64,
        limit_bytes: u64,
    },
    /// Scene mutation rate exceeds `max_update_rate_hz`.
    UpdateRateExceeded { current_hz: f32, limit_hz: f32 },
    /// A single tile contains more nodes than `max_nodes_per_tile`.
    NodeCountPerTileExceeded {
        tile_id_hint: String,
        current: u32,
        limit: u32,
    },
    /// Mutation would push texture memory past the absolute hard maximum.
    /// This is a critical violation — session is revoked immediately.
    CriticalTextureOomAttempt {
        requested_bytes: u64,
        hard_max_bytes: u64,
    },
    /// Session has accumulated too many protocol invariant violations.
    /// This is a critical violation — session is revoked immediately.
    RepeatedInvariantViolations { count: u32 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tile {
    pub id: SceneId,
    pub tab_id: SceneId,
    pub namespace: String,
    pub lease_id: SceneId,
    pub bounds: Rect,
    pub z_order: u32,
    pub opacity: f32,
    pub input_mode: InputMode,
    pub present_at: Option<u64>,
    pub expires_at: Option<u64>,
    pub resource_budget: ResourceBudget,
    pub root_node: Option<SceneId>,
    /// Visual overlay hint for the compositor.
    ///
    /// Set by the scene graph in response to lease state changes.
    /// The compositor renders the indicated badge/overlay within 1 frame
    /// of this field being set (spec line 133).
    #[serde(default)]
    pub visual_hint: crate::lease::TileVisualHint,
}

/// Runtime-owned scroll configuration for a tile-local viewport.
///
/// This is an ephemeral control surface used by local-first input processing.
/// It is not part of the durable scene snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TileScrollConfig {
    /// Whether horizontal scroll input is accepted.
    pub scrollable_x: bool,
    /// Whether vertical scroll input is accepted.
    pub scrollable_y: bool,
    /// Maximum horizontal offset in pixels from content origin.
    /// `None` disables x clamping.
    pub content_width: Option<f32>,
    /// Maximum vertical offset in pixels from content origin.
    /// `None` disables y clamping.
    pub content_height: Option<f32>,
}

impl TileScrollConfig {
    /// Convenience constructor for a vertically scrollable tile.
    pub fn vertical() -> Self {
        Self {
            scrollable_x: false,
            scrollable_y: true,
            content_width: None,
            content_height: None,
        }
    }
}

// ─── Nodes ──────────────────────────────────────────────────────────────────

/// How a node positions its direct children (hud-txkbh / hud-yfj8u).
///
/// `Absolute` (the default) is the pre-existing behavior: every child is drawn at
/// its own explicit `bounds.y`, exactly as before this field existed — so an
/// omitted or defaulted `layout` is byte-identical to the historical scene.
///
/// `VerticalFlow` asks the runtime to STACK the node's children vertically:
/// measure each child's wrapped rendered height and position the next child
/// directly below it plus a token-driven gap (see
/// `tze_hud_compositor::vertical_flow`). It exists so a publisher that cannot
/// measure wrapped text (the projection layer emitting one transcript node per
/// conversational turn) can leave child `bounds.y` unset and have the runtime
/// resolve the stack. Layout resolution runs in the runtime/compositor, never in
/// the model (LLM-out-of-frame-loop). The per-turn transcript split that would
/// drive this in production is gated on the Phase-1 Promotion Evidence Gate; the
/// mode is additive and default-off so the raw-tile pilot is unaffected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeLayout {
    /// Children are positioned by their own explicit `bounds` (historical
    /// behavior; the byte-compatible default).
    #[default]
    Absolute,
    /// Children are stacked vertically by the runtime's vertical-flow resolver.
    VerticalFlow,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: SceneId,
    pub children: Vec<SceneId>,
    pub data: NodeData,
    /// How this node lays out its direct children. Defaults to
    /// [`NodeLayout::Absolute`] (each child at its own `bounds.y`), so an
    /// existing node with no explicit layout renders identically to before this
    /// field existed. `#[serde(default)]` keeps older snapshots deserializable.
    #[serde(default)]
    pub layout: NodeLayout,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NodeData {
    SolidColor(SolidColorNode),
    TextMarkdown(TextMarkdownNode),
    HitRegion(HitRegionNode),
    StaticImage(StaticImageNode),
}

impl NodeData {
    /// The node's tile-local geometry (origin + extent relative to the parent
    /// tile's top-left). Every visual node variant carries its own `bounds`
    /// separate from `Tile::bounds`; the compositor combines the two only at
    /// render time (`tile.bounds.origin + node.bounds`).
    pub fn bounds(&self) -> Rect {
        match self {
            NodeData::SolidColor(n) => n.bounds,
            NodeData::TextMarkdown(n) => n.bounds,
            NodeData::HitRegion(n) => n.bounds,
            NodeData::StaticImage(n) => n.bounds,
        }
    }

    /// Mutable access to the node's tile-local geometry. Used by the viewer
    /// whole-portal resize to scale a tile's node tree in lock-step with the
    /// tile, so text wrap width (`TextMarkdownNode::bounds.width`, which the
    /// compositor uses as the layout column) re-resolves to the new geometry
    /// instead of staying pinned to the attach-time width.
    pub fn bounds_mut(&mut self) -> &mut Rect {
        match self {
            NodeData::SolidColor(n) => &mut n.bounds,
            NodeData::TextMarkdown(n) => &mut n.bounds,
            NodeData::HitRegion(n) => &mut n.bounds,
            NodeData::StaticImage(n) => &mut n.bounds,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SolidColorNode {
    pub color: Rgba,
    pub bounds: Rect,
    #[serde(default)]
    pub radius: Option<f32>,
}

/// A single inline color run for a [`TextMarkdownNode`].
///
/// Byte offsets are UTF-8 byte positions into the **raw** `content` string
/// (before any Markdown stripping).  The renderer remaps these offsets to
/// post-strip positions; if stripping is skipped when runs are present the
/// offsets are used as-is.
///
/// # Invariants (enforced at construction)
/// - `start_byte < end_byte`
/// - both `start_byte` and `end_byte` must be ≤ `content.len()`
/// - both must fall on valid UTF-8 character boundaries
///
/// # Overlap semantics
/// When runs overlap, **last-writer-wins** (the last run in the `Vec` whose
/// range covers a given byte position takes precedence).
///
/// Runs need not be pre-sorted. The compositor's `color_run_spans` helper
/// canonicalizes them into sorted, non-overlapping spans at render time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextColorRun {
    /// Inclusive UTF-8 byte offset into `TextMarkdownNode::content`.
    pub start_byte: u32,
    /// Exclusive UTF-8 byte offset into `TextMarkdownNode::content`.
    pub end_byte: u32,
    /// Color to apply to bytes `[start_byte, end_byte)`.
    pub color: Rgba,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextMarkdownNode {
    pub content: String,
    pub bounds: Rect,
    pub font_size_px: f32,
    pub font_family: FontFamily,
    pub color: Rgba,
    pub background: Option<Rgba>,
    pub alignment: TextAlign,
    pub overflow: TextOverflow,
    /// Optional inline color runs.
    ///
    /// Empty (default) → use `color` for the entire content.
    /// Non-empty → each run colors its `[start_byte, end_byte)` range;
    /// bytes not covered by any run fall back to `color`.
    /// Overlapping runs are resolved last-writer-wins (last entry wins).
    ///
    /// Stored as a boxed slice (16 bytes) rather than `Vec` (24 bytes) to keep
    /// `TextMarkdownNode` within the Node structural size budget (RFC 0001 §8).
    #[serde(default)]
    pub color_runs: Box<[TextColorRun]>,
}

/// Per-node visual style overrides applied locally without an agent roundtrip.
///
/// These are compositor-managed overrides — the agent provides the values; the
/// compositor applies them in real time based on `HitRegionLocalState`.
/// Source: RFC 0004 §7.1, input-model/spec.md line 249.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LocalStyle {
    /// Highlight tint applied while the pointer hovers over the node.
    /// `None` means no hover highlight.
    pub hover_tint: Option<Rgba>,
    /// Highlight tint applied while the node is pressed / active.
    pub pressed_tint: Option<Rgba>,
    /// Highlight tint applied while the node has keyboard focus.
    pub focus_outline_color: Option<Rgba>,
}

/// Hit-test spatial query result.
///
/// Returned by [`SceneGraph::hit_test`].  Represents the outcome of mapping a
/// 2D display-coordinate point to the deepest interactive scene element per the
/// traversal contract:
///
/// 1. Chrome layer tiles (lease priority 0) checked first.
/// 2. Content layer tiles in z-order descending; passthrough tiles skipped.
/// 3. Within each tile, nodes in reverse tree order (last sibling first, depth-first).
/// 4. Only [`NodeData::HitRegion`] nodes whose `bounds` contain the point qualify.
///
/// Source: RFC 0001 §5.1-5.2, scene-graph/spec.md lines 250-265,
///         input-model/spec.md lines 263-274.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HitResult {
    /// A [`NodeData::HitRegion`] node was hit.
    ///
    /// This is the most specific result: a named interactive region on a tile
    /// accepted the point.
    NodeHit {
        /// The tile that owns the hit node.
        tile_id: SceneId,
        /// The `HitRegionNode`'s scene ID.
        node_id: SceneId,
        /// The `interaction_id` string from the `HitRegionNode` — forwarded in
        /// all events dispatched from this hit.
        interaction_id: String,
    },
    /// A tile was hit but no `HitRegionNode` within it accepted the point.
    ///
    /// The tile itself absorbs the event (input_mode != Passthrough).
    TileHit {
        /// The tile that absorbed the point.
        tile_id: SceneId,
    },
    /// The point landed on a passthrough tile (or all tiles are passthrough at
    /// this coordinate).
    ///
    /// The event should be forwarded to the desktop in overlay mode or discarded
    /// in fullscreen.
    Passthrough,
    /// The point hit a runtime-managed zone interaction region (dismiss button
    /// or action button on a notification slot).
    ///
    /// These regions are managed by the compositor and do not require
    /// agent-owned tiles.  The `interaction_id` follows the scheme documented
    /// on [`ZoneHitRegion::interaction_id`].
    ZoneInteraction {
        /// Zone that owns the interactive element.
        zone_name: String,
        /// `published_at_wall_us` of the notification publication.
        published_at_wall_us: u64,
        /// Publisher namespace of the notification.
        publisher_namespace: String,
        /// Interaction identifier (dismiss or action callback id).
        interaction_id: String,
        /// What kind of interaction was hit.
        kind: ZoneInteractionKind,
    },
}

/// HitRegionNode is the sole interactive primitive in v1.
///
/// It defines a rectangular interactive region within a tile.  The runtime
/// performs hit-testing against `bounds` (tile-local coordinates) during
/// Stage 2 of the input dispatch pipeline.
///
/// # Local feedback
/// The runtime updates `HitRegionLocalState` (`hovered`, `pressed`) immediately
/// on hit, without waiting for the owning agent to acknowledge.  This satisfies
/// the "local feedback first" doctrine.
///
/// Source: RFC 0004 §7.1, RFC 0001 §2.4, input-model/spec.md lines 248-259.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HitRegionNode {
    /// Bounding rectangle in tile-local coordinates (origin = tile top-left).
    pub bounds: Rect,
    /// Agent-defined interaction identifier forwarded in all events from this node.
    /// Must be non-empty; the runtime treats an empty string as "unnamed".
    pub interaction_id: String,
    /// Whether this node participates in keyboard focus cycling.
    pub accepts_focus: bool,
    /// Whether this node accepts pointer events (hit-test only qualifies nodes
    /// where `accepts_pointer = true`).
    pub accepts_pointer: bool,
    /// When `true`, the runtime automatically acquires pointer capture for this
    /// node on PointerDownEvent without requiring an explicit CaptureRequest.
    /// Source: RFC 0004 §7.1 / input-model/spec.md line 142.
    #[serde(default)]
    pub auto_capture: bool,
    /// When `true`, pointer capture is released automatically on PointerUpEvent.
    /// Source: RFC 0004 §7.1 / input-model/spec.md line 120.
    #[serde(default)]
    pub release_on_up: bool,
    /// Tooltip text shown after the pointer has hovered for 500 ms.
    /// `None` means no tooltip.
    ///
    /// Boxed (alongside `composer_placeholder` below) to keep `HitRegionNode`
    /// within the 150-byte `Node` struct budget (scene-graph/spec.md line 302,
    /// RFC 0001 §8): a bare new `Option<String>`-sized field here ties
    /// `HitRegionNode`'s size with `TextMarkdownNode`'s and forces the
    /// `NodeData` enum discriminant out of its niche, growing `Node` past
    /// budget (verified empirically — see hud-se6hs).
    #[serde(default)]
    pub tooltip: Box<Option<String>>,
    /// When `true`, keystroke events targeting this focused node are intercepted
    /// by the runtime's `ComposerDraftManager` and routed into a `ComposerDraft`
    /// buffer instead of being forwarded to the owning agent as raw key events.
    ///
    /// The owning adapter receives coalesced `DraftStateNotification` state-stream
    /// messages and a final transactional `DraftSubmission` on Enter/submit.
    ///
    /// Source: spec §4.1 (hud-5jbra.4 / hud-qwqxy).
    #[serde(default)]
    pub accepts_composer_input: bool,
    /// Per-composer override for the empty-draft placeholder hint rendered by
    /// the compositor (`portal.composer.placeholder_color`, hud-evk0j).
    /// Ignored when `accepts_composer_input` is `false`.
    ///
    /// - `None` — inherit the runtime's built-in default hint for this
    ///   composer's type (e.g. the portal chat composer's "Type a message…" —
    ///   `COMPOSER_DEFAULT_PLACEHOLDER` in `tze_hud_runtime`).
    /// - `Some("")` — explicit opt-out: never show a placeholder for this
    ///   composer, even while the draft is empty.
    /// - `Some(text)` — show this exact hint instead of the runtime default.
    ///
    /// Boxed alongside `tooltip` above to stay within the `Node` struct's
    /// 150-byte budget.
    ///
    /// Follow-up to hud-evk0j (source: hud-se6hs).
    #[serde(default)]
    pub composer_placeholder: Box<Option<String>>,
    /// Compositor-applied visual style overrides for hover/press/focus states.
    ///
    /// Boxed to stay within the 150-byte `Node` struct budget
    /// (scene-graph/spec.md line 302, RFC 0001 §8).
    #[serde(default)]
    pub local_style: Box<LocalStyle>,
}

impl HitResult {
    /// Returns `true` if the point hit any interactive element (node, tile, or chrome).
    ///
    /// Returns `false` only for [`HitResult::Passthrough`].
    pub fn is_some(&self) -> bool {
        !matches!(self, HitResult::Passthrough)
    }

    /// Returns `true` if the point did not hit any interactive element.
    ///
    /// Equivalent to `!self.is_some()`.
    pub fn is_none(&self) -> bool {
        matches!(self, HitResult::Passthrough)
    }

    /// Returns `true` if this is a [`HitResult::NodeHit`].
    pub fn is_node_hit(&self) -> bool {
        matches!(self, HitResult::NodeHit { .. })
    }

    /// Extract the `(tile_id, node_id)` pair for `NodeHit` results.
    ///
    /// Returns `None` for all other variants.
    pub fn node_hit_ids(&self) -> Option<(SceneId, SceneId)> {
        if let HitResult::NodeHit {
            tile_id, node_id, ..
        } = self
        {
            Some((*tile_id, *node_id))
        } else {
            None
        }
    }

    /// Extract the tile_id for `NodeHit` or `TileHit` results.
    ///
    /// Returns `None` for `Chrome`, `ZoneInteraction`, and `Passthrough`.
    pub fn tile_id(&self) -> Option<SceneId> {
        match self {
            HitResult::NodeHit { tile_id, .. } | HitResult::TileHit { tile_id } => Some(*tile_id),
            _ => None,
        }
    }

    /// Returns `true` if this is a [`HitResult::ZoneInteraction`] hit.
    pub fn is_zone_interaction(&self) -> bool {
        matches!(self, HitResult::ZoneInteraction { .. })
    }
}

impl Default for HitRegionNode {
    fn default() -> Self {
        Self {
            bounds: Rect::new(0.0, 0.0, 0.0, 0.0),
            interaction_id: String::new(),
            accepts_focus: false,
            accepts_pointer: false,
            auto_capture: false,
            release_on_up: false,
            accepts_composer_input: false,
            tooltip: Box::default(),
            composer_placeholder: Box::default(),
            local_style: Box::default(),
        }
    }
}

/// A static image node that references a resource by its content-addressed identity.
///
/// Per resource-store/spec.md §Requirement: Ephemeral Storage in V1 (lines 244-246),
/// scene snapshots reference resources by `ResourceId` only — blob data is NOT
/// embedded.  On restart, the resource store is empty; agents must re-upload
/// referenced resources before the scene can fully render.
///
/// The `decoded_bytes` field is set at mutation time (from the resource store
/// record) so that the scene graph can enforce texture budget limits without
/// holding raw pixel data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StaticImageNode {
    /// Content-addressed identity of the image resource (BLAKE3 hash of raw bytes).
    ///
    /// This is the only reference to the backing data.  The blob itself lives in
    /// the runtime's ephemeral resource store, not in the scene graph.
    pub resource_id: ResourceId,
    /// Width of the image in pixels (metadata from upload).
    pub width: u32,
    /// Height of the image in pixels (metadata from upload).
    pub height: u32,
    /// Decoded in-memory size in bytes, recorded at mutation time for budget
    /// accounting.  Does NOT store the actual pixels.
    ///
    /// Using `u64` to match scene budget accounting types (`ResourceBudget.max_texture_bytes`)
    /// and the protobuf wire type, avoiding lossy casts on 32-bit targets.
    pub decoded_bytes: u64,
    /// How the image is fitted within `bounds`.
    pub fit_mode: ImageFitMode,
    /// Position and size within the parent tile (in tile-local coordinates).
    pub bounds: Rect,
}

// ─── Hit Region Local State (compositor-managed) ────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HitRegionLocalState {
    pub node_id: SceneId,
    pub hovered: bool,
    pub pressed: bool,
    pub focused: bool,
}

impl HitRegionLocalState {
    pub fn new(node_id: SceneId) -> Self {
        Self {
            node_id,
            hovered: false,
            pressed: false,
            focused: false,
        }
    }
}

// ─── Lease state machine (RFC 0008) ──────────────────────────────────────────

/// Lease lifecycle state per RFC 0008 SS3.
///
/// All 8 canonical states from the spec are present.
/// Terminal states (no further transitions): `Denied`, `Revoked`, `Expired`, `Released`.
/// Non-terminal: `Requested`, `Active`, `Suspended`, `Orphaned`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseState {
    /// Lease request received; runtime evaluating. No mutations allowed.
    Requested,
    /// Lease valid — agent holds mutation rights.
    Active,
    /// Lease suspended (safe mode) — mutations blocked, state and tiles preserved.
    Suspended,
    /// Session disconnected — within reconnect grace period. Tiles frozen.
    Orphaned,
    /// Lease request rejected — terminal; agent must submit a new request.
    Denied,
    /// Lease revoked — state destroyed. Terminal.
    Revoked,
    /// Lease expired (TTL exceeded) — state destroyed. Terminal.
    Expired,
    /// Agent voluntarily released lease — state destroyed. Terminal.
    Released,
}

impl LeaseState {
    /// Whether this state is terminal (no further transitions possible).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            LeaseState::Denied | LeaseState::Revoked | LeaseState::Expired | LeaseState::Released
        )
    }
}

/// Lease caps violation error.
#[derive(Clone, Debug, PartialEq)]
pub enum CapsError {
    /// Runtime-wide lease limit (64) exceeded — spec §Requirement: Lease Caps.
    MaxRuntimeLeasesExceeded { current: usize, limit: usize },
    /// Per-session lease hard limit (64) exceeded — spec §Requirement: Lease Caps.
    MaxSessionLeasesExceeded { current: usize, limit: usize },
    /// Tile-per-lease limit (64) exceeded — spec §Requirement: Lease Caps.
    MaxTilesPerLeaseExceeded { current: u32, limit: u32 },
    /// Node-per-tile limit (64) exceeded — spec §Requirement: Lease Caps.
    MaxNodesPerTileExceeded { current: u32, limit: u32 },
}

impl std::fmt::Display for CapsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapsError::MaxRuntimeLeasesExceeded { current, limit } => {
                write!(f, "MAX_RUNTIME_LEASES_EXCEEDED: {current} / {limit}")
            }
            CapsError::MaxSessionLeasesExceeded { current, limit } => {
                write!(f, "MAX_SESSION_LEASES_EXCEEDED: {current} / {limit}")
            }
            CapsError::MaxTilesPerLeaseExceeded { current, limit } => {
                write!(f, "MAX_TILES_PER_LEASE_EXCEEDED: {current} / {limit}")
            }
            CapsError::MaxNodesPerTileExceeded { current, limit } => {
                write!(f, "MAX_NODES_PER_TILE_EXCEEDED: {current} / {limit}")
            }
        }
    }
}

/// Error type for lease state transitions.
#[derive(Clone, Debug, PartialEq)]
pub enum LeaseError {
    /// Attempted an invalid state transition.
    InvalidTransition { from: LeaseState, to: LeaseState },
    /// Lease not found in the scene graph.
    LeaseNotFound(SceneId),
    /// Lease exists but is not in Active state.
    LeaseNotActive(SceneId),
    /// Mutation would exceed the lease's resource budget.
    BudgetExceeded(BudgetError),
    /// Lease caps exceeded (runtime-wide or per-session).
    CapsExceeded(CapsError),
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::InvalidTransition { from, to } => {
                write!(f, "invalid lease transition: {from:?} -> {to:?}")
            }
            LeaseError::LeaseNotFound(id) => write!(f, "lease not found: {id}"),
            LeaseError::LeaseNotActive(id) => write!(f, "lease not active: {id}"),
            LeaseError::BudgetExceeded(e) => write!(f, "budget exceeded: {e}"),
            LeaseError::CapsExceeded(e) => write!(f, "caps exceeded: {e}"),
        }
    }
}

impl std::error::Error for LeaseError {}

/// Error returned when a mutation batch would exceed budget limits.
#[derive(Clone, Debug, PartialEq)]
pub struct BudgetError {
    /// Which resource dimension was exceeded (e.g. "tiles", "texture_bytes").
    pub resource: String,
    /// Current usage before the mutation.
    pub current: u64,
    /// The configured limit.
    pub limit: u64,
    /// How much the mutation batch would add.
    pub requested: u64,
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: current={}, limit={}, requested={}",
            self.resource, self.current, self.limit, self.requested
        )
    }
}

/// Current resource usage for a lease.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResourceUsage {
    /// Number of tiles owned by this lease.
    pub tiles: u32,
    /// Total texture bytes across all tiles.
    pub texture_bytes: u64,
    /// Node count per tile (tile_id -> count).
    pub nodes_per_tile: HashMap<SceneId, u32>,
}

/// Information about an expired or cleaned-up lease.
#[derive(Clone, Debug, PartialEq)]
pub struct LeaseExpiry {
    /// The lease ID that was expired/cleaned up.
    pub lease_id: SceneId,
    /// The non-terminal state that transitioned into `terminal_state`.
    pub previous_state: LeaseState,
    /// The terminal state it entered.
    pub terminal_state: LeaseState,
    /// Tile IDs that were removed as a result.
    pub removed_tiles: Vec<SceneId>,
}

// ─── Lease ───────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    /// UUIDv7 lease identifier (time-ordered, SceneId type). Assigned at grant time.
    pub id: SceneId,
    /// Agent identity string (namespace). Established at session auth.
    pub namespace: String,
    /// Parent session identifier. Lease is invalidated if session is revoked.
    pub session_id: SceneId,
    pub state: LeaseState,
    /// Wall-clock grant timestamp in milliseconds since Unix epoch (RFC 0003 wall-clock domain).
    /// Corresponds to `granted_at_wall_us / 1000` in the wire protocol.
    pub granted_at_ms: u64,
    pub ttl_ms: u64,
    pub resource_budget: ResourceBudget,
    /// Spatial constraints on tiles owned by this lease, enforced at
    /// interactive resize time (gesture + hotkey).  `0.0` for either field
    /// means unconstrained.
    #[serde(default)]
    pub spatial_budget: TileSpatialBudget,
    // Suspension tracking
    /// Timestamp when the lease was suspended (ms since epoch).
    pub suspended_at_ms: Option<u64>,
    /// TTL remaining at the moment of suspension (ms).
    pub ttl_remaining_at_suspend_ms: Option<u64>,
    // Orphan/disconnect tracking
    /// Timestamp when the agent disconnected (ms since epoch).
    pub disconnected_at_ms: Option<u64>,
    /// Grace period before an orphaned lease is cleaned up (ms). Default 30_000.
    pub grace_period_ms: u64,
}

impl Lease {
    /// Check if the lease has expired based on effective TTL elapsed.
    ///
    /// Accounts for suspension: time spent in Suspended state does not count
    /// toward TTL consumption (RFC 0008 SS4.3).
    pub fn is_expired(&self, now_ms: u64) -> bool {
        match self.state {
            // Terminal states are already past expiry semantics.
            LeaseState::Denied
            | LeaseState::Revoked
            | LeaseState::Expired
            | LeaseState::Released => true,
            // When suspended, TTL clock is paused — not expired.
            LeaseState::Suspended => false,
            // When orphaned, TTL continues.
            // (Grace period handles cleanup separately.)
            LeaseState::Orphaned | LeaseState::Active | LeaseState::Requested => {
                self.effective_remaining_ms(now_ms) == 0
            }
        }
    }

    /// Remaining TTL in milliseconds (0 if expired).
    ///
    /// If the lease was previously suspended, the suspension duration is
    /// deducted so that the effective TTL is preserved across suspend/resume.
    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.effective_remaining_ms(now_ms)
    }

    /// Effective remaining TTL accounting for suspension pauses.
    fn effective_remaining_ms(&self, now_ms: u64) -> u64 {
        match self.state {
            LeaseState::Suspended => {
                // TTL frozen at the value saved when suspension started.
                self.ttl_remaining_at_suspend_ms.unwrap_or(0)
            }
            _ => {
                let expires = self.granted_at_ms + self.ttl_ms;
                expires.saturating_sub(now_ms)
            }
        }
    }

    // ─── State transition methods ────────────────────────────────────────

    /// Transition Active -> Suspended (safe mode entry).
    ///
    /// Pauses the TTL clock and records suspension timestamp.
    pub fn suspend(&mut self, now_ms: u64) -> Result<(), LeaseError> {
        if self.state != LeaseState::Active {
            return Err(LeaseError::InvalidTransition {
                from: self.state,
                to: LeaseState::Suspended,
            });
        }
        let remaining = self.effective_remaining_ms(now_ms);
        self.suspended_at_ms = Some(now_ms);
        self.ttl_remaining_at_suspend_ms = Some(remaining);
        self.state = LeaseState::Suspended;
        Ok(())
    }

    /// Transition Suspended -> Active (safe mode exit).
    ///
    /// Resumes the TTL clock. The `granted_at_ms` and `ttl_ms` are adjusted
    /// so that the remaining TTL equals what was saved at suspension time.
    pub fn resume(&mut self, now_ms: u64) -> Result<(), LeaseError> {
        if self.state != LeaseState::Suspended {
            return Err(LeaseError::InvalidTransition {
                from: self.state,
                to: LeaseState::Active,
            });
        }
        // Restore TTL: set granted_at_ms so that granted_at_ms + ttl_ms
        // equals now_ms + remaining.
        if let Some(remaining) = self.ttl_remaining_at_suspend_ms {
            self.granted_at_ms = now_ms;
            self.ttl_ms = remaining;
        }
        self.suspended_at_ms = None;
        self.ttl_remaining_at_suspend_ms = None;
        self.state = LeaseState::Active;
        Ok(())
    }

    /// Transition Active -> Orphaned (agent disconnect).
    ///
    /// Starts the grace period. TTL continues running.
    /// Only `Active` is accepted as source state; any other state returns `InvalidTransition`.
    pub fn disconnect(&mut self, now_ms: u64) -> Result<(), LeaseError> {
        if self.state != LeaseState::Active {
            return Err(LeaseError::InvalidTransition {
                from: self.state,
                to: LeaseState::Orphaned,
            });
        }
        self.disconnected_at_ms = Some(now_ms);
        self.state = LeaseState::Orphaned;
        Ok(())
    }

    /// Transition Orphaned -> Active (agent reconnect within grace period).
    pub fn reconnect(&mut self, now_ms: u64) -> Result<(), LeaseError> {
        if self.state != LeaseState::Orphaned {
            return Err(LeaseError::InvalidTransition {
                from: self.state,
                to: LeaseState::Active,
            });
        }
        // Check that grace period has not expired.
        if self.check_grace_expired(now_ms) {
            return Err(LeaseError::InvalidTransition {
                from: self.state,
                to: LeaseState::Active,
            });
        }
        self.disconnected_at_ms = None;
        self.state = LeaseState::Active;
        Ok(())
    }

    /// Transition any non-terminal state -> Revoked.
    pub fn revoke(&mut self) -> Result<(), LeaseError> {
        if self.state.is_terminal() {
            return Err(LeaseError::InvalidTransition {
                from: self.state,
                to: LeaseState::Revoked,
            });
        }
        self.state = LeaseState::Revoked;
        Ok(())
    }

    /// Whether the lease is currently in Active state.
    pub fn is_active(&self) -> bool {
        self.state == LeaseState::Active
    }

    /// Whether mutations are allowed. Only Active state permits mutations.
    pub fn is_mutations_allowed(&self) -> bool {
        self.state == LeaseState::Active
    }

    /// Check if the grace period has expired for an orphaned lease.
    pub fn check_grace_expired(&self, now_ms: u64) -> bool {
        match (self.state, self.disconnected_at_ms) {
            (LeaseState::Orphaned, Some(disc_at)) => now_ms >= disc_at + self.grace_period_ms,
            _ => false,
        }
    }

    /// Check if a suspended lease has exceeded the maximum suspension time.
    pub fn check_suspension_expired(&self, now_ms: u64, max_suspend_ms: u64) -> bool {
        match (self.state, self.suspended_at_ms) {
            (LeaseState::Suspended, Some(susp_at)) => now_ms >= susp_at + max_suspend_ms,
            _ => false,
        }
    }
}

// ─── Zone types ─────────────────────────────────────────────────────────────

/// Minimum z-order for Content-layer zone tiles (= 0x8000_0000).
///
/// Content-layer zone tiles must participate in the same z-order traversal as
/// agent tiles but in the reserved upper band (≥ ZONE_TILE_Z_MIN). Agent tiles
/// must use z_order values below this constant.
///
/// Per scene-graph/spec.md §Requirement: Zone Layer Attachment.
pub const ZONE_TILE_Z_MIN: u32 = 0x8000_0000;

/// Layer attachment for a zone instance — determines rendering order.
///
/// Per RFC 0001 §2.5 and scene-graph/spec.md line 241.
///
/// The default is `Content` (within content-layer z-order space).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LayerAttachment {
    /// Rendered behind all agent tiles (below content layer).
    Background,
    /// Rendered within the content layer z-order space at
    /// z_order >= [`ZONE_TILE_Z_MIN`].
    #[default]
    Content,
    /// Rendered above all agent content; managed by runtime chrome rendering.
    Chrome,
}

/// Display edge for edge-anchored zone geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayEdge {
    Top,
    Bottom,
    Left,
    Right,
}

/// Geometry policy — how a zone is positioned on the display.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum GeometryPolicy {
    /// Percentage-based position relative to display area.
    Relative {
        x_pct: f32,
        y_pct: f32,
        width_pct: f32,
        height_pct: f32,
    },
    /// Anchored to a display edge.
    EdgeAnchored {
        edge: DisplayEdge,
        /// Used for Top/Bottom edges.
        height_pct: f32,
        /// Used for Left/Right edges.
        width_pct: f32,
        margin_px: f32,
    },
}

/// Resolve geometry with four-tier precedence:
/// user override > agent-requested > config override > default policy.
pub fn resolve_geometry_override_chain(
    user_override: Option<GeometryPolicy>,
    agent_requested: Option<GeometryPolicy>,
    config_override: Option<GeometryPolicy>,
    default_policy: Option<GeometryPolicy>,
) -> Option<GeometryPolicy> {
    user_override
        .or(agent_requested)
        .or(config_override)
        .or(default_policy)
}

/// Convert absolute pixel bounds into a display-relative geometry policy.
///
/// When display dimensions are non-positive, this falls back to a zero geometry.
pub fn rect_to_relative_geometry_policy(
    bounds: Rect,
    display_width: f32,
    display_height: f32,
) -> GeometryPolicy {
    if display_width <= 0.0 || display_height <= 0.0 {
        return GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.0,
            height_pct: 0.0,
        };
    }

    GeometryPolicy::Relative {
        x_pct: bounds.x / display_width,
        y_pct: bounds.y / display_height,
        width_pct: bounds.width / display_width,
        height_pct: bounds.height / display_height,
    }
}

/// Resolve a geometry policy into absolute pixel bounds for the given display size.
pub fn geometry_policy_to_absolute_rect(
    policy: GeometryPolicy,
    display_width: f32,
    display_height: f32,
) -> Rect {
    match policy {
        GeometryPolicy::Relative {
            x_pct,
            y_pct,
            width_pct,
            height_pct,
        } => Rect::new(
            x_pct * display_width,
            y_pct * display_height,
            width_pct * display_width,
            height_pct * display_height,
        ),
        GeometryPolicy::EdgeAnchored {
            edge,
            height_pct,
            width_pct,
            margin_px,
        } => {
            let margin = margin_px.max(0.0);
            match edge {
                DisplayEdge::Top => {
                    let h = (height_pct * display_height).max(0.0);
                    let w = (display_width - 2.0 * margin).max(0.0);
                    Rect::new(margin, margin, w, h)
                }
                DisplayEdge::Bottom => {
                    let h = (height_pct * display_height).max(0.0);
                    let w = (display_width - 2.0 * margin).max(0.0);
                    Rect::new(margin, (display_height - h - margin).max(0.0), w, h)
                }
                DisplayEdge::Left => {
                    let w = (width_pct * display_width).max(0.0);
                    let h = (display_height - 2.0 * margin).max(0.0);
                    Rect::new(margin, margin, w, h)
                }
                DisplayEdge::Right => {
                    let w = (width_pct * display_width).max(0.0);
                    let h = (display_height - 2.0 * margin).max(0.0);
                    Rect::new((display_width - w - margin).max(0.0), margin, w, h)
                }
            }
        }
    }
}

/// Media types that can be published to a zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ZoneMediaType {
    /// Stream-text with optional breakpoints.
    StreamText,
    /// Notification: text + icon + urgency.
    ShortTextWithIcon,
    /// Status-bar: key-value map.
    KeyValuePairs,
    /// Static image resource.
    StaticImage,
    /// Solid color fill.
    SolidColor,
}

/// Visual pattern rendered inside a drag handle affordance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DragHandleGripPattern {
    /// Render three small grip dots.
    Dots,
    /// Render a thin horizontal grip bar.
    Bar,
    /// Render no interior grip decoration.
    None,
}

/// Design token key for the portal header-band drag-handle height (hud-643dv).
///
/// The runtime resolves this from the display token map; when absent the
/// compositor falls back to [`PORTAL_HEADER_DRAG_BAND_PX_DEFAULT`]. Operators can
/// override it to make the draggable titlebar band taller/shorter without a code
/// change (CLAUDE.md "never hardcode visual properties in the compositor").
pub const PORTAL_HEADER_DRAG_BAND_TOKEN: &str = "portal.header.drag_band_px";

/// Fallback height (px) of the portal header drag band when the
/// [`PORTAL_HEADER_DRAG_BAND_TOKEN`] token is not set. Chosen to match the
/// exemplar's header strip height so the invisible drag band lines up with the
/// visible header chrome.
pub const PORTAL_HEADER_DRAG_BAND_PX_DEFAULT: f32 = 52.0;

/// Design token key for the minimum legible font size (px) when whole-portal
/// resize scales text down (hud-ovjxu.1, spec §Portal Resize Text Scaling).
/// The compositor reads this from the display token map; when absent it falls
/// back to [`PORTAL_TEXT_MIN_FONT_PX_DEFAULT`]. Never hardcode the clamp in the
/// compositor (CLAUDE.md "never hardcode visual properties in the compositor").
pub const PORTAL_TEXT_MIN_FONT_PX_TOKEN: &str = "portal.text.min_font_px";

/// Design token key for the maximum legible font size (px) when whole-portal
/// resize scales text up. Fallback: [`PORTAL_TEXT_MAX_FONT_PX_DEFAULT`].
pub const PORTAL_TEXT_MAX_FONT_PX_TOKEN: &str = "portal.text.max_font_px";

/// Fallback minimum legible font size (px) for resize text scaling. Small enough
/// to let a shrunk portal keep several lines visible, large enough to stay
/// readable; below this the font clamps and only the content window shrinks.
pub const PORTAL_TEXT_MIN_FONT_PX_DEFAULT: f32 = 9.0;

/// Fallback maximum legible font size (px) for resize text scaling. Caps how
/// large text grows on a grown portal so a huge window does not produce
/// absurdly large glyphs.
pub const PORTAL_TEXT_MAX_FONT_PX_DEFAULT: f32 = 48.0;

/// Design-token bundle for runtime drag handle visuals.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DragHandleStyle {
    /// Base fill color.
    pub color: Rgba,
    /// Opacity while idle (no hover/press).
    pub opacity_idle: f32,
    /// Opacity while hovered or pressed.
    pub opacity_active: f32,
    /// Handle width in density-independent pixels.
    pub width_dp: f32,
    /// Handle height in density-independent pixels.
    pub height_dp: f32,
    /// Corner radius in pixels.
    pub border_radius: f32,
    /// Grip decoration style.
    pub grip_pattern: DragHandleGripPattern,
}

impl Default for DragHandleStyle {
    fn default() -> Self {
        Self {
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            opacity_idle: 0.4,
            opacity_active: 1.0,
            width_dp: 24.0,
            height_dp: 8.0,
            border_radius: 4.0,
            grip_pattern: DragHandleGripPattern::Dots,
        }
    }
}

/// Rendering policy — how content is presented in the zone.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RenderingPolicy {
    pub font_size_px: Option<f32>,
    pub backdrop: Option<Rgba>,
    pub text_align: Option<TextAlign>,
    pub margin_px: Option<f32>,
    /// Font family for zone text rendering.
    #[serde(default)]
    pub font_family: Option<FontFamily>,
    /// Font weight (CSS-style: 100–900); None = compositor default (400).
    #[serde(default)]
    pub font_weight: Option<u16>,
    /// Primary text color; None = compositor default (white).
    #[serde(default)]
    pub text_color: Option<Rgba>,
    /// Backdrop opacity override (0.0–1.0); None = use `backdrop` alpha.
    #[serde(default)]
    pub backdrop_opacity: Option<f32>,
    /// Outline/border color for the zone frame; None = no outline.
    #[serde(default)]
    pub outline_color: Option<Rgba>,
    /// Outline/border width in pixels; None = no outline.
    #[serde(default)]
    pub outline_width: Option<f32>,
    /// Horizontal margin in pixels (left + right); None = compositor default.
    #[serde(default)]
    pub margin_horizontal: Option<f32>,
    /// Vertical margin in pixels (top + bottom); None = compositor default.
    #[serde(default)]
    pub margin_vertical: Option<f32>,
    /// Duration of the enter/reveal transition in milliseconds; None = no transition.
    #[serde(default)]
    pub transition_in_ms: Option<u32>,
    /// Duration of the exit/dismiss transition in milliseconds; None = no transition.
    #[serde(default)]
    pub transition_out_ms: Option<u32>,
    /// Text overflow mode; None = falls back to Clip.
    #[serde(default)]
    pub overflow: Option<TextOverflow>,
    /// Status-bar key-to-icon SVG mapping.
    ///
    /// Maps merge keys (e.g., `"weather"`, `"battery"`) to SVG file paths or
    /// resource IDs used to render an icon alongside the text value.
    ///
    /// Keys absent from this map are rendered as text-only (backward compatible).
    /// SVG path values are opaque strings resolved by the compositor's resource
    /// loader; they MUST NOT contain unresolved config-layer token placeholders
    /// such as `{{icon.battery}}` — those are resolved at profile load time
    /// before being stored here.
    ///
    /// Only meaningful for zones with `accepted_media_types: [KeyValuePairs]`
    /// (i.e., the `status-bar` zone). Ignored for all other zone types.
    #[serde(default)]
    pub key_icon_map: HashMap<String, String>,
    /// Corner radius for the zone backdrop in pixels.
    ///
    /// When set, the compositor uses the SDF rounded-rectangle pipeline instead
    /// of the axis-aligned quad pipeline to render this zone's backdrop.
    /// The value is clamped to `[0, min(half_width, half_height)]` at render
    /// time so it never exceeds the geometry.
    ///
    /// `None` (the default) means axis-aligned flat rect (existing behaviour).
    #[serde(default)]
    pub backdrop_radius: Option<f32>,
    /// Tail-anchored truncation opt-in for `ZoneContent::StreamText` content.
    ///
    /// `Some(true)` makes a streaming zone show the **newest** content (the tail)
    /// when text overflows the zone bounds — mirroring the text-stream transcript
    /// portal's follow-tail behaviour.  `None` / `Some(false)` (the default)
    /// preserves head-anchored truncation, which pins the **oldest** content.
    ///
    /// Only meaningful when [`Self::overflow`] resolves to
    /// [`TextOverflow::Ellipsis`]: head-anchored truncation shows the first
    /// `max_lines` runs, tail-anchored shows the last `max_lines` runs.  For
    /// `Clip` overflow this field has no effect (clipping always shows the head).
    ///
    /// Only consulted for `ZoneContent::StreamText`; notification and status-bar
    /// content always render head-anchored.
    ///
    /// Populated from zone configuration / design tokens at profile load time;
    /// never hardcoded in the compositor.
    #[serde(default)]
    pub stream_tail_anchored: Option<bool>,
}

/// Contention policy — what happens when multiple agents publish to the same zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentionPolicy {
    /// Most recent publish replaces previous content.
    LatestWins,
    /// Publishes accumulate as a stack; each auto-dismisses.
    Stack { max_depth: u8 },
    /// Each publish includes a key; same key replaces, different keys coexist.
    MergeByKey { max_keys: u8 },
    /// Only one occupant; new publish evicts current one.
    Replace,
}

/// Full zone definition per RFC 0001 §2.5.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ZoneDefinition {
    pub id: SceneId,
    pub name: String,
    pub description: String,
    pub geometry_policy: GeometryPolicy,
    pub accepted_media_types: Vec<ZoneMediaType>,
    pub rendering_policy: RenderingPolicy,
    pub contention_policy: ContentionPolicy,
    pub max_publishers: u32,
    /// Auto-clear timeout in milliseconds; None = no auto-clear.
    pub auto_clear_ms: Option<u64>,
    /// When true, publishes to this zone are fire-and-forget (no ZonePublishResult).
    /// When false (default), publishes are transactional and receive a ZonePublishResult.
    /// Per RFC 0005 §3.1, §8.6.
    #[serde(default)]
    pub ephemeral: bool,
    /// Layer attachment — determines rendering order and z-space.
    /// Defaults to [`LayerAttachment::Content`] if not specified.
    #[serde(default)]
    pub layer_attachment: LayerAttachment,
}

// ─── Zone publish token ──────────────────────────────────────────────────────

/// Opaque capability token that authorizes publishing to a specific zone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZonePublishToken {
    /// Opaque bytes issued at session auth.
    pub token: Vec<u8>,
}

// ─── Zone content ────────────────────────────────────────────────────────────

/// A single actionable button on a notification.
///
/// When the user clicks or activates an action button, the runtime routes the
/// callback to the publishing agent by emitting an interaction event.  The
/// emitted event's `interaction_id` follows the scheme
/// `"zone:{zone_name}:action:{published_at_wall_us}:{publisher_namespace}:{callback_id}"`
/// (see [`ZoneInteractionKind::Action`] and [`ZoneHitRegion::interaction_id`]).
///
/// Rendering: buttons appear in a horizontal row at the bottom of the
/// notification slot.  Labels exceeding `MAX_ACTION_LABEL_LEN` characters are
/// not currently truncated by the runtime — callers should enforce this limit
/// before publishing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationAction {
    /// Human-readable label shown on the button (e.g. "Open", "Snooze").
    ///
    /// Maximum `MAX_ACTION_LABEL_LEN` characters.  The runtime does not
    /// currently enforce truncation; callers are responsible.
    pub label: String,
    /// Opaque callback identifier forwarded to the publishing agent when the
    /// button is activated (click or keyboard Enter/Space).
    ///
    /// Must be non-empty.  The runtime treats an empty `callback_id` as
    /// "unnamed" and will still route the event, but agents should use
    /// meaningful identifiers for routing clarity.
    pub callback_id: String,
}

/// Maximum UTF-8 character length for a `NotificationAction` label.
///
/// Labels exceeding this limit may overflow the rendered slot.  The runtime
/// does not currently enforce truncation at publish time; callers should
/// respect this limit before constructing a [`NotificationAction`].
pub const MAX_ACTION_LABEL_LEN: usize = 32;

/// Notification payload: text + optional icon + urgency + optional two-line layout + optional action buttons.
///
/// ## Single-line vs. two-line rendering
///
/// - When `title` is empty (or absent), the notification renders as a single
///   line using the `text` field (existing behavior, fully backward compatible).
/// - When `title` is non-empty, the notification renders as two lines:
///   - Line 1: `title` — bold weight (`typography.notification.title.weight`,
///     default 700), `font_size_px` from `RenderingPolicy` / design tokens.
///   - Line 2: `text` — regular weight (400), 0.85× the title font size.
///
///   The slot height is expanded to fit both lines plus inter-line spacing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NotificationPayload {
    /// Body text.  For single-line notifications this is the only displayed
    /// text.  For two-line notifications this is the second (body) line.
    pub text: String,
    /// Resource name or empty string.
    pub icon: String,
    /// 0=low, 1=normal, 2=urgent, 3=critical.
    pub urgency: u32,
    /// Per-publication TTL in milliseconds.
    ///
    /// When `Some`, the compositor begins a 150ms fade-out this many milliseconds
    /// after the publication is first rendered.  When `None`, the zone's
    /// `auto_clear_ms` is used as the default TTL (typically 8 000 ms for the
    /// `notification-area` zone).
    #[serde(default)]
    pub ttl_ms: Option<u64>,
    /// Optional bold title for two-line notification layout.
    ///
    /// When non-empty: renders as the first (title) line in bold, with `text`
    /// rendered as the second (body) line in regular weight.
    /// When empty or absent: single-line rendering using `text` only.
    #[serde(default)]
    pub title: String,
    /// Optional action buttons shown at the bottom of the notification slot.
    ///
    /// At most `MAX_NOTIFICATION_ACTIONS` actions are rendered; excess entries
    /// are silently ignored by the compositor.  Each action's label is
    /// truncated to `MAX_ACTION_LABEL_LEN` characters.
    ///
    /// When empty (the default), no action buttons are rendered.
    #[serde(default)]
    pub actions: Vec<NotificationAction>,
}

impl NotificationPayload {
    /// No title and no body: it would render as an empty card. Both publish
    /// planes reject it with `INVALID_ARGUMENT`.
    pub fn is_blank(&self) -> bool {
        self.title.trim().is_empty() && self.text.trim().is_empty()
    }
}

/// Hint for a rejected blank notification, shared by both planes.
pub const BLANK_NOTIFICATION_HINT: &str = "notification needs a title or body";

/// Maximum number of action buttons rendered per notification slot.
pub const MAX_NOTIFICATION_ACTIONS: usize = 3;

// ─── Zone hit regions ────────────────────────────────────────────────────────

/// Element kind within a notification slot's interactive region.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ZoneInteractionKind {
    /// The dismiss (×) button in the top-right corner of a notification slot.
    ///
    /// Activating this removes the notification from the zone.  The
    /// `interaction_id` for dismiss elements follows the pattern
    /// `"zone:{zone_name}:dismiss:{published_at_wall_us}:{publisher_namespace}"`.
    Dismiss,
    /// An action button.  `callback_id` is the agent-defined identifier from
    /// [`NotificationAction::callback_id`].  The `interaction_id` follows the
    /// pattern
    /// `"zone:{zone_name}:action:{published_at_wall_us}:{publisher_namespace}:{callback_id}"`.
    Action { callback_id: String },
    /// Runtime chrome drag handle for a movable element.
    DragHandle {
        /// Element identity in scene-id wire format.
        element_id: SceneId,
        /// Element class this handle belongs to.
        element_kind: DragHandleElementKind,
        /// `true` when this is a portal header-BAND handle (Windows-titlebar
        /// strip) rather than the legacy small grip. A band is an unambiguous
        /// drag surface, so the runtime activates it IMMEDIATELY on press+move —
        /// no 250 ms long-press hold and no early-movement cancel — while the grip
        /// keeps the long-press hysteresis that disambiguates tap-to-focus from
        /// drag (hud-cpjqe).
        is_header_band: bool,
    },
    /// Runtime "jump to latest" pill shown over a portal tile that is
    /// scrolled away from the tail of its content (hud-9ci61).
    ///
    /// Activating this snaps the tile's viewport back to the tail via
    /// `ScrollState::reset_to_tail`. The `interaction_id` follows the pattern
    /// `"jump-to-latest:{tile_id}"`.
    JumpToLatest {
        /// Tile the pill belongs to.
        tile_id: SceneId,
    },
    /// Runtime close button shown on a hovered agent tile (hud-jm8nq.11).
    ///
    /// Activating this is a viewer override: the tile's lease is revoked and its
    /// agent receives `Reclaimed{OVERRIDE}`. The `interaction_id` follows the
    /// pattern `"tile-close:{tile_id}"`.
    DismissTile {
        /// Tile the button belongs to.
        tile_id: SceneId,
    },
}

/// Element class for runtime drag handle interactions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DragHandleElementKind {
    Tile,
    Zone,
    Widget,
}

/// A runtime-managed interactive region derived from zone content layout.
///
/// Zone content (e.g. notification slots) is rendered by the compositor, which
/// also computes the pixel-space bounds of interactive affordances such as
/// dismiss buttons and action buttons.  These bounds are written back into
/// `SceneGraph::overlay.zone_hit_regions` each frame so that the hit-test pipeline can
/// route pointer and keyboard events to zone interactions without requiring
/// agent-owned tiles.
///
/// `ZoneHitRegion`s are ephemeral: they are recomputed every frame by the
/// compositor and discarded when the zone has no active content.  They MUST NOT
/// be serialised alongside the scene graph — use `#[serde(skip)]` on the
/// owning field.
///
/// # Input routing contract
///
/// When [`SceneGraph::hit_test`] finds no tile hit at a point, it falls through
/// to the zone hit region list.  The first region whose `bounds` contain the
/// display-space point produces a [`HitResult::ZoneInteraction`] result.
///
/// Per RFC 0004 §7.1 doctrine: HitRegionNode is the sole interactive
/// primitive; zone hit regions are a thin adapter that maps zone geometry
/// to the same `interaction_id`-based routing model without requiring
/// agent-managed tiles for each notification slot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ZoneHitRegion {
    /// Zone that owns this interactive element.
    pub zone_name: String,
    /// `published_at_wall_us` of the publication this region belongs to.
    pub published_at_wall_us: u64,
    /// Publisher namespace of the publication.
    pub publisher_namespace: String,
    /// Display-space bounding rectangle (absolute pixel coordinates).
    pub bounds: Rect,
    /// What kind of interaction this region represents.
    pub kind: ZoneInteractionKind,
    /// Interaction identifier forwarded in all events from this region.
    ///
    /// Scheme:
    /// - Dismiss: `"zone:{zone_name}:dismiss:{published_at_wall_us}:{publisher_namespace}"`
    /// - Action:  `"zone:{zone_name}:action:{published_at_wall_us}:{publisher_namespace}:{callback_id}"`
    pub interaction_id: String,
    /// Tab-order index within the zone's interactive elements.
    ///
    /// Zone hit regions participate in keyboard focus cycling alongside
    /// tile-owned HitRegionNodes.  The compositor assigns tab-order indices
    /// in top-to-bottom, left-to-right reading order (dismiss first, then
    /// actions in order of their `Vec` position).
    pub tab_order: u32,
}

/// Runtime-managed chrome-layer drag handle region for movable elements.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DragHandleHitRegion {
    /// Element identity targeted by the drag handle.
    pub element_id: SceneId,
    /// Element class for routing and debug surfaces.
    pub element_kind: DragHandleElementKind,
    /// Display-space bounds for hit-testing and rendering.
    pub bounds: Rect,
    /// Interaction identifier (`drag-handle:<element_id_hex>`).
    pub interaction_id: String,
    /// Runtime-internal hit-region node contract for this handle.
    pub hit_region: HitRegionNode,
    /// Stable tab-order index across all drag handles.
    pub tab_order: u32,
    /// When `true`, this is a portal **header-band** drag handle (the whole top
    /// header strip of a portal frame, Windows-titlebar style) rather than the
    /// legacy small centered grip.  A band spans a large area that legitimately
    /// overlaps interactive controls (e.g. the minimize button), so — unlike the
    /// grip — it yields to any `accepts_pointer` HitRegionNode under the point:
    /// the band drags empty header space but a control on the band still wins
    /// (hud-643dv).  Legacy grips leave this `false` and keep their original
    /// chrome-priority precedence.
    #[serde(default)]
    pub is_header_band: bool,
}

/// Local-first hover/press state for chrome drag handles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DragHandleLocalState {
    pub hovered: bool,
    pub pressed: bool,
}

/// Runtime-managed chrome context menu anchored to a drag handle.
///
/// Shown on right-click (desktop) or short-tap (touch) of a drag handle.
/// Contains a single "Reset to default" action.  Auto-dismisses after 3 s or
/// on click-outside.
///
/// Stored in `SceneGraph::overlay.drag_handle_context_menu` (ephemerally; never
/// serialised).  The compositor renders it as a chrome-layer overlay and
/// registers its pixel bounds in `context_menu_hit_rect` so the input path
/// can route a left-click to the reset action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DragHandleContextMenuState {
    /// Element to reset when the user activates the menu item.
    pub element_id: SceneId,
    /// Display-space anchor point (top-left of menu).
    pub anchor_x: f32,
    pub anchor_y: f32,
    /// Monotonic nanosecond timestamp when the menu was shown.
    /// Used for the 3-second auto-dismiss timer.
    pub shown_at_ns: u64,
    /// Pixel-space bounding rect of the "Reset to default" button, populated
    /// by the compositor after each render pass (used for hit-testing).
    #[serde(skip, default)]
    pub reset_button_rect: Option<Rect>,
}

/// Status-bar payload: key → display string map.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusBarPayload {
    pub entries: HashMap<String, String>,
}

/// Content that can be published to a zone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ZoneContent {
    StreamText(String),
    Notification(NotificationPayload),
    StatusBar(StatusBarPayload),
    SolidColor(Rgba),
    /// Static image reference (v1-mandatory: content-addressed resource).
    StaticImage(ResourceId),
}

// ─── Zone publish records ────────────────────────────────────────────────────

/// Record of a single publish event into a zone.
///
/// This is the publication event (third level of the zone ontology:
/// ZoneType → ZoneInstance → ZonePublication → ZoneOccupancy).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ZonePublishRecord {
    pub zone_name: String,
    pub publisher_namespace: String,
    pub content: ZoneContent,
    /// UTC wall-clock timestamp in microseconds (per timing-model/spec.md Clock Domain Naming Convention).
    pub published_at_wall_us: u64,
    /// For MergeByKey contention: the key under which this record is stored.
    pub merge_key: Option<String>,
    /// Optional wall-clock expiry timestamp (microseconds since epoch).
    /// When present, the runtime MUST clear this publication at or before this time.
    /// None = no expiry (publication lives until explicitly cleared or zone cleared).
    pub expires_at_wall_us: Option<u64>,
    /// Optional content classification tag (e.g., "public", "private", "pii").
    /// Used by policy and redaction layers; treated as opaque by the scene graph.
    pub content_classification: Option<String>,
    /// Optional byte-offset breakpoints for `StreamText` word-by-word reveal.
    ///
    /// When non-empty, the compositor reveals text progressively: it shows text
    /// up to `breakpoints[i]` bytes at frame `i`, then advances to the next
    /// breakpoint on subsequent frames.  Breakpoints identify word boundaries in
    /// the UTF-8 text string (byte offsets, not character indices).
    ///
    /// When empty (default), the full text is revealed immediately.
    ///
    /// Only meaningful when `content` is `ZoneContent::StreamText`.  Ignored for
    /// all other content types.
    ///
    /// `u64` is used (rather than `usize`) so that the wire format is stable
    /// across 32-bit and 64-bit platforms.  Callers convert to `usize` at
    /// indexing time (e.g., `bp as usize`).
    #[serde(default)]
    pub breakpoints: Vec<u64>,
    /// The lease this publication was made under. A terminal lease clears
    /// exactly the publications carrying its id; `None` belongs to no lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<SceneId>,
}

/// A zone instance — zone type bound to a specific tab.
///
/// In v1, zone instances are static (loaded from config; one instance per tab
/// per zone type). Agents MUST NOT create zone instances.
///
/// Per scene-graph/spec.md §Requirement: Zone Registry (line 185).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ZoneInstance {
    /// The zone type (definition) this instance belongs to.
    pub zone_type_name: String,
    /// The tab this instance is bound to.
    pub tab_id: SceneId,
    /// Instance-level geometry override (None = use zone type's geometry_policy).
    pub geometry_override: Option<GeometryPolicy>,
}

/// Zone occupancy — the resolved state after applying the contention policy.
///
/// This is the fourth level of the zone ontology. In v1, effective_geometry
/// is NOT exposed (deferred to post-v1 per spec line 360).
///
/// Per scene-graph/spec.md §Requirement: Zone Occupancy Query API (line 360,
/// post-v1), and §Requirement: Zone Registry (line 185, v1-mandatory).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ZoneOccupancy {
    pub zone_name: String,
    pub tab_id: SceneId,
    /// Active publications after applying contention policy.
    pub active_publications: Vec<ZonePublishRecord>,
    /// Occupant count after contention resolution.
    pub occupant_count: u32,
    // NOTE: effective_geometry intentionally absent in v1 (post-v1 per spec line 360).
}

// ─── Zone registry ───────────────────────────────────────────────────────────

/// Snapshot of the zone registry (all zones + active publishes).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ZoneRegistrySnapshot {
    pub zones: Vec<ZoneDefinition>,
    pub active_publishes: Vec<ZonePublishRecord>,
}

// ─── Widget types ────────────────────────────────────────────────────────────

/// Minimum z-order for widget tiles. Widget tiles appear above zone tiles when
/// they overlap spatially. Per widget-system spec §Requirement: Widget Contention
/// and Governance.
pub const WIDGET_TILE_Z_MIN: u32 = 0x9000_0000;

/// Parameter types supported by the widget system in v1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WidgetParamType {
    /// IEEE 754 32-bit float, range-clamped to [min, max].
    F32,
    /// UTF-8 string, max 1024 bytes by default.
    String,
    /// RGBA color as 4x u8 in [0, 255].
    Color,
    /// Enumerated string value from a declared allowed-values set.
    Enum,
}

/// A typed parameter value for a widget parameter.
///
/// Invariants:
/// - `F32` values MUST be finite (no NaN, no infinity).
/// - `String` values MUST be at most 1024 UTF-8 bytes.
/// - `Enum` values MUST match one of the `allowed_values` in the
///   corresponding `WidgetParamConstraints`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WidgetParameterValue {
    F32(f32),
    String(std::string::String),
    /// RGBA color as 4x u8 in [0, 255].
    Color(Rgba),
    Enum(std::string::String),
}

/// Constraints for a widget parameter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct WidgetParamConstraints {
    /// Minimum value (f32 parameters only). None = unconstrained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub f32_min: Option<f32>,
    /// Maximum value (f32 parameters only). None = unconstrained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub f32_max: Option<f32>,
    /// Maximum UTF-8 byte length for string parameters. 0 / None = default 1024.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub string_max_bytes: Option<u32>,
    /// Allowed values for enum parameters. Empty = unconstrained.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enum_allowed_values: Vec<std::string::String>,
}

/// A single parameter declaration in a widget's parameter schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetParameterDeclaration {
    pub name: std::string::String,
    pub param_type: WidgetParamType,
    pub default_value: WidgetParameterValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<WidgetParamConstraints>,
}

/// How a parameter value is mapped to an SVG attribute.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WidgetBindingMapping {
    /// f32 parameter: maps [min, max] range to [attr_min, attr_max] via linear interpolation.
    Linear { attr_min: f32, attr_max: f32 },
    /// String and Color parameters: use the value as-is.
    Direct,
    /// Enum parameters: maps each enum value to a specific attribute value.
    Discrete {
        value_map: std::collections::BTreeMap<std::string::String, std::string::String>,
    },
}

/// A single parameter-to-SVG-attribute binding.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetBinding {
    /// Parameter name from the widget's parameter schema.
    pub param: std::string::String,
    /// SVG element ID within the layer's SVG file.
    pub target_element: std::string::String,
    /// SVG attribute name (or the synthetic target `"text-content"`).
    pub target_attribute: std::string::String,
    pub mapping: WidgetBindingMapping,
}

/// An SVG layer in a widget type definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetSvgLayer {
    /// Filename within the bundle directory.
    pub svg_file: std::string::String,
    pub bindings: Vec<WidgetBinding>,
}

/// A normalized rectangle in widget-local coordinates.
///
/// Coordinates are relative fractions in `[0.0, 1.0]` where:
/// - `x_pct`, `y_pct` are the top-left origin
/// - `width_pct`, `height_pct` are the rectangle size
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetNormalizedRect {
    pub x_pct: f32,
    pub y_pct: f32,
    pub width_pct: f32,
    pub height_pct: f32,
}

/// Runtime-managed hover behavior for a widget instance.
///
/// When configured, the runtime tracks cursor dwell inside `trigger_rect` and
/// writes `visibility_param` to `visible_value` after `delay_ms`. When cursor
/// leaves the region, it writes `hidden_value`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetHoverBehavior {
    pub trigger_rect: WidgetNormalizedRect,
    pub delay_ms: u32,
    pub visibility_param: std::string::String,
    pub hidden_value: f32,
    pub visible_value: f32,
}

/// Full widget type definition (the first level of the widget ontology).
///
/// Widget types are registered at startup from asset bundles and are
/// immutable after registration. Agents MUST NOT create widget types.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetDefinition {
    /// Kebab-case unique id matching `[a-z][a-z0-9-]*`.
    pub id: std::string::String,
    pub name: std::string::String,
    pub description: std::string::String,
    pub parameter_schema: Vec<WidgetParameterDeclaration>,
    pub layers: Vec<WidgetSvgLayer>,
    pub default_geometry_policy: GeometryPolicy,
    pub default_rendering_policy: RenderingPolicy,
    pub default_contention_policy: ContentionPolicy,
    /// Maximum number of active publications per publisher namespace.
    ///
    /// Mirrors `ZoneDefinition::max_publishers`. Enforced by `publish_to_widget`
    /// under `Stack` contention (the only policy where multiple records from the
    /// same namespace can coexist). Defaults to `u32::MAX` (unbounded) for
    /// backward compatibility when deserializing older widget definitions.
    #[serde(default = "WidgetDefinition::default_max_publishers")]
    pub max_publishers: u32,
    /// When true, publishes to this widget are fire-and-forget (no WidgetPublishResult).
    #[serde(default)]
    pub ephemeral: bool,
    /// Optional runtime-managed hover behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hover_behavior: Option<WidgetHoverBehavior>,
}

impl WidgetDefinition {
    /// Default value for `max_publishers` when deserializing older definitions
    /// that predate the field.  `u32::MAX` means "unbounded" and preserves the
    /// behavior of every existing widget definition that did not set the field.
    pub fn default_max_publishers() -> u32 {
        u32::MAX
    }
}

/// A widget instance — a widget type bound to a specific tab.
///
/// Widget instances are static in v1 (loaded from config; not agent-created).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetInstance {
    /// Stable element ID for this widget instance.
    #[serde(default = "SceneId::null")]
    pub id: SceneId,
    /// References `WidgetDefinition.id`.
    pub widget_type_name: std::string::String,
    /// The tab this instance is bound to.
    pub tab_id: SceneId,
    /// Instance-level geometry override (None = use type's default_geometry_policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry_override: Option<GeometryPolicy>,
    /// Instance-level contention override (None = use type's default_contention_policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contention_override: Option<ContentionPolicy>,
    /// Addressing key: explicit instance_id from config, or widget_type_name if absent.
    pub instance_name: std::string::String,
    /// Current effective parameter values (HashMap for runtime use).
    pub current_params: HashMap<std::string::String, WidgetParameterValue>,
}

/// A recorded widget publication (the third level of the widget ontology).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetPublishRecord {
    /// Instance addressing key.
    pub widget_name: std::string::String,
    pub publisher_namespace: std::string::String,
    pub params: HashMap<std::string::String, WidgetParameterValue>,
    /// UTC wall-clock timestamp in microseconds since Unix epoch.
    pub published_at_wall_us: u64,
    /// For MergeByKey contention: the key under which this record is stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_key: Option<std::string::String>,
    /// Optional expiry timestamp (microseconds since epoch). None = no expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_wall_us: Option<u64>,
    pub transition_ms: u32,
    /// The lease this publication was made under (see `ZonePublishRecord`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<SceneId>,
}

/// Resolved occupancy state for a widget instance after contention policy.
///
/// This is the fourth level of the widget ontology. The compositor reads
/// `effective_params` to determine current visual property values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetOccupancy {
    /// Instance addressing key.
    pub widget_name: std::string::String,
    pub tab_id: SceneId,
    pub active_publications: Vec<WidgetPublishRecord>,
    pub occupant_count: u32,
    /// Resolved parameters after contention policy; falls back to
    /// `WidgetDefinition` defaults when no publications are active.
    pub effective_params: HashMap<std::string::String, WidgetParameterValue>,
}

// ─── Widget registry ─────────────────────────────────────────────────────────

/// Runtime-owned widget registry, parallel to ZoneRegistry.
///
/// Populated at startup from asset bundles (widget types) and
/// configuration (widget instances). Read-only from the agent perspective.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WidgetRegistry {
    /// Widget type definitions keyed by widget id.
    pub definitions: HashMap<std::string::String, WidgetDefinition>,
    /// Widget instances keyed by instance_name.
    pub instances: HashMap<std::string::String, WidgetInstance>,
    /// Active publishes per widget instance_name.
    pub active_publishes: HashMap<std::string::String, Vec<WidgetPublishRecord>>,
    /// Runtime-registered widget SVG asset handles keyed by `"{type}:{svg_file}"`.
    ///
    /// This tracks stage-1 asset registration state so publish paths can stay
    /// lightweight (parameter-only).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub runtime_svg_handles: HashMap<std::string::String, std::string::String>,
}

impl WidgetRegistry {
    pub fn new() -> Self {
        Self {
            definitions: HashMap::new(),
            instances: HashMap::new(),
            active_publishes: HashMap::new(),
            runtime_svg_handles: HashMap::new(),
        }
    }

    fn runtime_svg_key(widget_type_id: &str, svg_filename: &str) -> String {
        format!("{widget_type_id}:{svg_filename}")
    }

    /// Register a widget definition. Overwrites any existing definition with the same id.
    pub fn register_definition(&mut self, def: WidgetDefinition) {
        self.definitions.insert(def.id.clone(), def);
    }

    /// Register a widget instance. Overwrites any existing instance with the same instance_name.
    pub fn register_instance(&mut self, instance: WidgetInstance) {
        self.instances
            .insert(instance.instance_name.clone(), instance);
    }

    /// Look up a widget definition by id.
    pub fn get_definition(&self, id: &str) -> Option<&WidgetDefinition> {
        self.definitions.get(id)
    }

    /// Register or update the runtime asset handle for a widget SVG layer.
    pub fn register_runtime_svg_handle(
        &mut self,
        widget_type_id: &str,
        svg_filename: &str,
        asset_handle: &str,
    ) {
        let key = Self::runtime_svg_key(widget_type_id, svg_filename);
        self.runtime_svg_handles
            .insert(key, asset_handle.to_string());
    }

    /// Retrieve a previously registered runtime SVG handle.
    pub fn runtime_svg_handle(&self, widget_type_id: &str, svg_filename: &str) -> Option<&str> {
        let key = Self::runtime_svg_key(widget_type_id, svg_filename);
        self.runtime_svg_handles.get(&key).map(String::as_str)
    }

    /// Look up a widget instance by instance_name.
    pub fn get_instance(&self, instance_name: &str) -> Option<&WidgetInstance> {
        self.instances.get(instance_name)
    }

    /// Resolve a widget instance geometry policy using override precedence:
    /// user override > instance geometry override > widget default policy.
    pub fn resolve_geometry_policy_for_instance(
        &self,
        instance_name: &str,
        user_override: Option<&GeometryPolicy>,
    ) -> Option<GeometryPolicy> {
        let instance = self.instances.get(instance_name)?;
        let definition = self.definitions.get(&instance.widget_type_name)?;

        resolve_geometry_override_chain(
            user_override.copied(),
            None,
            instance.geometry_override,
            Some(definition.default_geometry_policy),
        )
    }

    /// Get the current active publish(es) for a widget instance.
    pub fn active_for_widget(&self, instance_name: &str) -> &[WidgetPublishRecord] {
        self.active_publishes
            .get(instance_name)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Query occupancy for a widget instance (resolved state after contention policy).
    ///
    /// Returns `None` if the instance is not found.
    ///
    /// # effective_params resolution
    ///
    /// `effective_params` is computed by applying the widget's contention policy
    /// to the current set of active publications:
    ///
    /// - **LatestWins**: the sole active publication's params are merged over
    ///   the schema defaults.
    /// - **Stack**: the top-of-stack (most recent, i.e. last) publication's
    ///   params are merged over the schema defaults.
    /// - **MergeByKey**: each active publication holds the most recent value for
    ///   its key; all publications' params are merged over schema defaults in
    ///   insertion order (later entries win on overlap).
    /// - **Replace**: the sole active publication's params are used as-is,
    ///   without falling back to schema defaults for missing keys.
    ///
    /// When no publications are active, `effective_params` always falls back to
    /// the schema defaults regardless of policy.
    pub fn get_occupancy(&self, instance_name: &str, tab_id: SceneId) -> Option<WidgetOccupancy> {
        let instance = self.instances.get(instance_name)?;
        let def = self.definitions.get(&instance.widget_type_name)?;
        let pubs = self
            .active_publishes
            .get(instance_name)
            .cloned()
            .unwrap_or_default();
        let occupant_count = pubs.len() as u32;

        let contention_policy = instance
            .contention_override
            .unwrap_or(def.default_contention_policy);

        // Build schema defaults lazily (helper closure); only branches that
        // merge over defaults call this.  Replace does not need defaults, so
        // we avoid the allocation in that fast path.
        let make_defaults = || -> HashMap<std::string::String, WidgetParameterValue> {
            def.parameter_schema
                .iter()
                .map(|p| (p.name.clone(), p.default_value.clone()))
                .collect()
        };

        let effective_params = if pubs.is_empty() {
            // No active publications — always use schema defaults.
            make_defaults()
        } else {
            match contention_policy {
                ContentionPolicy::LatestWins => {
                    // Only one publication is retained by publish_to_widget;
                    // merge it over defaults.
                    let mut params = make_defaults();
                    params.extend(pubs[0].params.clone());
                    params
                }
                ContentionPolicy::Stack { .. } => {
                    // Publications are ordered oldest-first (new entries pushed to back).
                    // Top-of-stack = last element = most recent publication.
                    let mut params = make_defaults();
                    if let Some(top) = pubs.last() {
                        params.extend(top.params.clone());
                    }
                    params
                }
                ContentionPolicy::MergeByKey { .. } => {
                    // One record per key, each already holding the most recent value
                    // for that key.  Merge all publications' params over defaults;
                    // later entries in the vec win on key overlap (consistent with
                    // insertion order maintained by publish_to_widget).
                    let mut params = make_defaults();
                    for pub_record in &pubs {
                        params.extend(pub_record.params.clone());
                    }
                    params
                }
                ContentionPolicy::Replace => {
                    // Replace policy: most recent publication's params are used
                    // as-is.  No fallback to defaults for missing keys — the
                    // publication completely replaces prior state.
                    pubs[0].params.clone()
                }
            }
        };

        Some(WidgetOccupancy {
            widget_name: instance_name.to_string(),
            tab_id,
            active_publications: pubs,
            occupant_count,
            effective_params,
        })
    }

    /// Snapshot the registry (all definitions + instances + all active publishes).
    pub fn snapshot(&self) -> WidgetRegistrySnapshot {
        WidgetRegistrySnapshot {
            widget_types: self.definitions.values().cloned().collect(),
            widget_instances: self.instances.values().cloned().collect(),
            active_publishes: self
                .active_publishes
                .values()
                .flat_map(|v| v.iter().cloned())
                .collect(),
        }
    }
}

impl Default for WidgetRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of the widget registry (all types + instances + active publishes).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetRegistrySnapshot {
    pub widget_types: Vec<WidgetDefinition>,
    pub widget_instances: Vec<WidgetInstance>,
    pub active_publishes: Vec<WidgetPublishRecord>,
}

/// Deterministic snapshot of the widget registry for inclusion in SceneGraphSnapshot.
///
/// Uses BTreeMap/sorted Vec for deterministic iteration order per RFC 0001 §4.1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneGraphWidgetRegistry {
    /// Widget definitions sorted by id for deterministic serialization.
    pub widget_types: std::collections::BTreeMap<std::string::String, WidgetDefinition>,
    /// Widget instances sorted by instance_name for determinism.
    pub widget_instances: Vec<WidgetInstance>,
    /// Active publications sorted by instance_name then publisher_namespace for determinism.
    pub active_publications:
        std::collections::BTreeMap<std::string::String, Vec<WidgetPublishRecord>>,
}

// ─── Scene Snapshot ──────────────────────────────────────────────────────────

/// Deterministic snapshot of the zone registry for inclusion in SceneGraphSnapshot.
///
/// Uses BTreeMap/sorted Vec for deterministic iteration order per RFC 0001 §4.1.
/// MUST NOT include effective_geometry (post-v1 per spec line 360).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneGraphZoneRegistry {
    /// Zone definitions sorted by zone name for deterministic serialization.
    pub zone_types: std::collections::BTreeMap<String, ZoneDefinition>,
    /// Zone instances sorted by zone_type_name then tab_id for determinism.
    pub zone_instances: Vec<ZoneInstance>,
    /// Active publications sorted by zone name then publisher_namespace for determinism.
    /// Includes zone publications but MUST NOT include effective_geometry (post-v1).
    pub active_publications: std::collections::BTreeMap<String, Vec<ZonePublishRecord>>,
}

/// Full deterministic scene snapshot at a specific sequence number.
///
/// Implements the v1 snapshot semantics from RFC 0001 §4.1 and §4.2:
/// - Complete, deterministic serialization at a single point in time
/// - All maps use BTreeMap for deterministic iteration order
/// - BLAKE3 checksum over the canonical serialized content (excluding the
///   checksum field itself, see [`SceneGraphSnapshot::compute_checksum`])
///
/// # Determinism
/// Given identical scene state, two calls to [`SceneGraph::take_snapshot`]
/// at the same sequence number MUST produce byte-identical output.
///
/// # v1 Scope Constraints
/// - Resources are ephemeral: snapshot references ResourceIds but MUST NOT
///   embed blob data (resource-store/spec.md §Requirement: Ephemeral Storage)
/// - effective_geometry is NOT included (post-v1, spec line 360)
/// - Incremental diff is NOT available (post-v1, spec line 342)
///
/// # Reconnection
/// When an agent reconnects, the runtime MUST send a full SceneGraphSnapshot.
/// The agent discards prior state and resumes from `sequence`.
///
/// Source: RFC 0001 §4.1, §4.2, §6, §10.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneGraphSnapshot {
    /// Sequence number at the time this snapshot was taken (RFC 0001 §3.5).
    pub sequence: u64,

    /// UTC wall-clock at snapshot time, microseconds since Unix epoch.
    pub snapshot_wall_us: u64,

    /// Monotonic timestamp at snapshot time, microseconds since process start.
    pub snapshot_mono_us: u64,

    /// All tabs, ordered by display_order (BTreeMap keyed by display_order for
    /// stable iteration; value is the Tab with its SceneId).
    pub tabs: std::collections::BTreeMap<u32, Tab>,

    /// All tiles, keyed by SceneId (BTreeMap for deterministic iteration order).
    pub tiles: std::collections::BTreeMap<SceneId, Tile>,

    /// All nodes, keyed by SceneId (BTreeMap for deterministic iteration order).
    pub nodes: std::collections::BTreeMap<SceneId, Node>,

    /// First-class text-stream portal surface descriptors, keyed by host tile id
    /// (BTreeMap for deterministic iteration order; RFC 0013 §7.2, hud-tc153).
    ///
    /// These are runtime overlay state (`RuntimeOverlayState::portal_surfaces`,
    /// `#[serde(skip)]`) rather than scene nodes, so they are otherwise invisible
    /// to the serialized graph. Including them here lets a reconnecting resident
    /// session recover its declared surfaces from the snapshot instead of
    /// re-declaring blindly (hud-ruynm reconnect parity).
    ///
    /// Visibility mirrors [`tiles`](Self::tiles): a surface is keyed by its host
    /// tile id, so a session filtering the snapshot to the tiles it owns keeps
    /// exactly the surfaces on those tiles and no others. A `PortalPart.node` that
    /// was nulled by [`revalidate_portal_surface_part_nodes`] after a transcript
    /// republish is serialized as `null` faithfully — the snapshot never fabricates
    /// a node reference.
    ///
    /// [`revalidate_portal_surface_part_nodes`]: crate::graph::SceneGraph::revalidate_portal_surface_part_nodes
    ///
    /// Serde: `default` on read (older snapshots that predate this field
    /// deserialize to an empty map) and `skip_serializing_if` empty on write, so
    /// an empty map is omitted from the canonical JSON. This keeps the checksum
    /// bytes byte-identical to a pre-field snapshot, so
    /// [`verify_checksum`](Self::verify_checksum) still succeeds for older
    /// surface-less snapshots instead of failing on a spurious `"portal_surfaces":{}`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub portal_surfaces: std::collections::BTreeMap<SceneId, PortalSurface>,

    /// Zone registry snapshot: types, instances, active publications.
    /// Does NOT include effective_geometry (post-v1, spec line 360).
    pub zone_registry: SceneGraphZoneRegistry,

    /// Widget registry snapshot: types, instances, active publications.
    /// Populated from WidgetRegistry via deterministic BTreeMap ordering.
    pub widget_registry: SceneGraphWidgetRegistry,

    /// The currently active tab, or None if no tab is active.
    pub active_tab: Option<SceneId>,

    /// Display area used for tile bounds validation and viewport-relative placement.
    #[serde(default = "default_snapshot_display_area")]
    pub display_area: Rect,

    /// BLAKE3 checksum (32 bytes as hex) of the canonical serialized content.
    ///
    /// Computed over the JSON-serialized bytes of this struct with the
    /// `checksum` field set to the empty string. See [`SceneGraphSnapshot::verify_checksum`].
    pub checksum: String,
}

fn default_snapshot_display_area() -> Rect {
    Rect::new(0.0, 0.0, 1920.0, 1080.0)
}

impl SceneGraphSnapshot {
    /// Compute the BLAKE3 checksum of the canonical snapshot content.
    ///
    /// The checksum is computed over the JSON-serialized bytes of this snapshot
    /// with the `checksum` field set to the empty string `""`. This ensures the
    /// checksum is computed over the content, not over itself.
    ///
    /// The returned value is a 64-character lowercase hex string.
    ///
    /// # Protocol
    /// 1. Clone this snapshot with `checksum = String::new()`.
    /// 2. Serialize to compact JSON (no pretty-printing for byte stability).
    /// 3. Compute BLAKE3 hash of the UTF-8 bytes.
    /// 4. Encode as lowercase hex.
    pub fn compute_checksum(&self) -> String {
        // Build a version with empty checksum to hash
        let mut canonical = self.clone();
        canonical.checksum = String::new();
        let json = serde_json::to_string(&canonical)
            .expect("SceneGraphSnapshot serialization must not fail");
        let hash = blake3::hash(json.as_bytes());
        hash.to_hex().to_string()
    }

    /// Verify the embedded checksum matches the snapshot content.
    ///
    /// Returns `true` if the stored `checksum` matches the result of
    /// [`Self::compute_checksum`].
    pub fn verify_checksum(&self) -> bool {
        let expected = self.compute_checksum();
        self.checksum == expected
    }

    /// Serialize this snapshot to compact JSON.
    ///
    /// Uses compact (non-pretty) JSON for byte stability. The same scene state
    /// will always produce the same bytes.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Deserialize a snapshot from JSON.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// Runtime-owned zone registry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZoneRegistry {
    /// Zone definitions keyed by zone name.
    pub zones: HashMap<String, ZoneDefinition>,
    /// Active publishes per zone name.
    /// For LatestWins/Replace: at most one entry per zone.
    /// For Stack: ordered oldest-first, bounded by max_depth.
    /// For MergeByKey: keyed by merge_key, bounded by max_keys.
    pub active_publishes: HashMap<String, Vec<ZonePublishRecord>>,
}

impl ZoneRegistry {
    pub fn new() -> Self {
        Self {
            zones: HashMap::new(),
            active_publishes: HashMap::new(),
        }
    }

    /// Create a registry pre-populated with the default v1 zones.
    ///
    /// V1 zone set (scene-graph/spec.md §Implementation Details):
    /// subtitle, notification-area, status-bar, pip, ambient-background, alert-banner.
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();

        // 1. status-bar: edge-anchored bottom, MergeByKey, Chrome layer
        registry.register(ZoneDefinition {
            id: SceneId::new(),
            name: "status-bar".to_string(),
            description: "Status bar — right edge, vertical layout, chrome layer".to_string(),
            geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.92,
                y_pct: 0.10,
                width_pct: 0.07,
                height_pct: 0.40,
            },
            accepted_media_types: vec![ZoneMediaType::KeyValuePairs],
            rendering_policy: RenderingPolicy::default(),
            contention_policy: ContentionPolicy::MergeByKey { max_keys: 32 },
            max_publishers: 16,
            auto_clear_ms: None,
            ephemeral: false,
            layer_attachment: LayerAttachment::Chrome,
        });

        // 2. notification-area: top-right relative, Stack, Chrome layer
        registry.register(ZoneDefinition {
            id: SceneId::new(),
            name: "notification-area".to_string(),
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
            auto_clear_ms: Some(8_000),
            ephemeral: false,
            layer_attachment: LayerAttachment::Chrome,
        });

        // 3. subtitle: edge-anchored bottom, LatestWins, Content layer
        registry.register(ZoneDefinition {
            id: SceneId::new(),
            name: "subtitle".to_string(),
            description: "Subtitle / caption overlay".to_string(),
            geometry_policy: GeometryPolicy::EdgeAnchored {
                edge: DisplayEdge::Bottom,
                height_pct: 0.10,
                width_pct: 0.80,
                margin_px: 48.0,
            },
            accepted_media_types: vec![ZoneMediaType::StreamText],
            rendering_policy: RenderingPolicy::default(),
            contention_policy: ContentionPolicy::LatestWins,
            max_publishers: 1,
            auto_clear_ms: None,
            ephemeral: false,
            layer_attachment: LayerAttachment::Content,
        });

        // 4. pip: picture-in-picture, Relative geometry, Replace, Content layer
        registry.register(ZoneDefinition {
            id: SceneId::new(),
            name: "pip".to_string(),
            description: "Picture-in-picture overlay zone".to_string(),
            geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.75,
                y_pct: 0.70,
                width_pct: 0.22,
                height_pct: 0.26,
            },
            accepted_media_types: vec![ZoneMediaType::SolidColor, ZoneMediaType::StaticImage],
            rendering_policy: RenderingPolicy::default(),
            contention_policy: ContentionPolicy::Replace,
            max_publishers: 1,
            auto_clear_ms: None,
            ephemeral: false,
            layer_attachment: LayerAttachment::Content,
        });

        // 5. ambient-background: full-screen, Replace, Background layer
        registry.register(ZoneDefinition {
            id: SceneId::new(),
            name: "ambient-background".to_string(),
            description: "Ambient background zone — full display, behind all content".to_string(),
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
            auto_clear_ms: None,
            ephemeral: false,
            layer_attachment: LayerAttachment::Background,
        });

        // 6. alert-banner: edge-anchored top, Stack-by-severity, Chrome layer.
        //
        // Heading typography per spec §Alert-Banner Heading Typography:
        //   font_size_px = 24px (typography.heading.size)
        //   font_family  = SystemSansSerif (typography.heading.family)
        //   font_weight  = 700/bold (typography.heading.weight)
        //   text_color   = #FFFFFF white (color.text.primary) — max contrast vs severity backdrops
        //   margin_horizontal = 8px inset from backdrop edges
        //
        // Chrome-layer positioning per spec §Alert-Banner Chrome-Layer Positioning:
        //   layer_attachment = Chrome — renders above all agent content
        //   width_pct = 1.0 — full display width
        //   height_pct = 0.06 — nominal single-slot height for edge anchoring and debug geometry.
        //   Runtime slot height is derived from RenderingPolicy (stack_slot_height); total stack
        //   height = active_count × slot_h, not height_pct × screen_height.
        //   Zero-height when inactive: compositor skips backdrop/text for empty zones;
        //   no visible pixels are emitted when no alerts are active.
        //
        // Multiple banners stack vertically ordered by severity (critical at top,
        // warning below, info at bottom).  Within the same severity level, newer
        // banners appear above older ones.  Zone height grows dynamically:
        // slot_height × active_count; zero height when no banners are active.
        //
        // backdrop + backdrop_opacity provide the dark fallback color for non-severity
        // content (e.g. StreamText) and are overridden by severity token colors for
        // NotificationPayload in render_zone_content.
        registry.register(ZoneDefinition {
            id: SceneId::new(),
            name: "alert-banner".to_string(),
            description: "Alert banner — top edge, severity-stacked multi-occupant, chrome layer, heading typography".to_string(),
            geometry_policy: GeometryPolicy::EdgeAnchored {
                edge: DisplayEdge::Top,
                // 6% of display height: at 720p this gives 43.2px, comfortably above the 24px heading.
                height_pct: 0.06,
                width_pct: 1.0,
                margin_px: 0.0,
            },
            accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon, ZoneMediaType::StreamText],
            rendering_policy: RenderingPolicy {
                // Heading typography — §Alert-Banner Heading Typography
                font_size_px: Some(24.0),
                font_family: Some(FontFamily::SystemSansSerif),
                font_weight: Some(700),
                // White text (#FFFFFF) — max contrast against severity backdrops
                text_color: Some(Rgba {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                }),
                // Dark backdrop fallback (used for non-Notification content + default)
                backdrop: Some(Rgba {
                    r: 0.1,
                    g: 0.1,
                    b: 0.16,
                    a: 0.9,
                }),
                backdrop_opacity: Some(0.9),
                // Horizontal inset from backdrop edges
                margin_horizontal: Some(8.0),
                // Flush to anchored edge — no vertical margin
                margin_vertical: Some(0.0),
                ..RenderingPolicy::default()
            },
            contention_policy: ContentionPolicy::Stack { max_depth: 8 },
            // max_publishers is enforced per publisher_namespace (one active banner
            // per agent).  max_depth=8 allows up to 8 simultaneous banners from
            // 8 different agents; keeping max_publishers=1 ensures no single agent
            // can flood the stack.
            max_publishers: 1,
            auto_clear_ms: Some(10_000),
            ephemeral: false,
            layer_attachment: LayerAttachment::Chrome,
        });

        registry
    }

    /// Register a zone definition. Overwrites any existing definition with the same name.
    pub fn register(&mut self, zone: ZoneDefinition) {
        self.zones.insert(zone.name.clone(), zone);
    }

    /// Remove a zone definition by name. Returns the removed definition if present.
    pub fn unregister(&mut self, name: &str) -> Option<ZoneDefinition> {
        self.active_publishes.remove(name);
        self.zones.remove(name)
    }

    /// Look up a zone by name.
    pub fn get_by_name(&self, name: &str) -> Option<&ZoneDefinition> {
        self.zones.get(name)
    }

    /// Resolve a zone geometry policy using override precedence:
    /// user override > config override > zone default policy.
    ///
    /// In v1, zone defaults are represented by `ZoneDefinition.geometry_policy`.
    /// When `config_override` is `None`, the zone default remains the fallback.
    pub fn resolve_geometry_policy_for_zone(
        &self,
        zone_name: &str,
        user_override: Option<&GeometryPolicy>,
        config_override: Option<&GeometryPolicy>,
    ) -> Option<GeometryPolicy> {
        let zone = self.zones.get(zone_name)?;
        resolve_geometry_override_chain(
            user_override.copied(),
            None,
            config_override.copied(),
            Some(zone.geometry_policy),
        )
    }

    /// Query zones that accept a given media type.
    pub fn zones_accepting(&self, media_type: ZoneMediaType) -> Vec<&ZoneDefinition> {
        self.zones
            .values()
            .filter(|z| z.accepted_media_types.contains(&media_type))
            .collect()
    }

    /// Return all zone definitions.
    pub fn all_zones(&self) -> Vec<&ZoneDefinition> {
        self.zones.values().collect()
    }

    /// Get the current active publish(es) for a zone.
    pub fn active_for_zone(&self, zone_name: &str) -> &[ZonePublishRecord] {
        self.active_publishes
            .get(zone_name)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Query occupancy for a zone instance (resolved state after contention policy).
    ///
    /// In v1, active publishes are global (not tab-scoped); `tab_id` is echoed
    /// through to the returned `ZoneOccupancy` but is NOT used as a filter.
    /// Tab-scoped zone instances are a post-v1 feature. In v1, `effective_geometry`
    /// is also not exposed (deferred to post-v1 per spec line 360).
    ///
    /// Returns `None` if the zone is not found.
    pub fn get_occupancy(&self, zone_name: &str, tab_id: SceneId) -> Option<ZoneOccupancy> {
        let _zone = self.zones.get(zone_name)?;
        let pubs = self
            .active_publishes
            .get(zone_name)
            .cloned()
            .unwrap_or_default();
        let occupant_count = pubs.len() as u32;
        Some(ZoneOccupancy {
            zone_name: zone_name.to_string(),
            tab_id,
            active_publications: pubs,
            occupant_count,
        })
    }

    /// Snapshot the registry (all definitions + all active publishes).
    pub fn snapshot(&self) -> ZoneRegistrySnapshot {
        ZoneRegistrySnapshot {
            zones: self.zones.values().cloned().collect(),
            active_publishes: self
                .active_publishes
                .values()
                .flat_map(|v| v.iter().cloned())
                .collect(),
        }
    }
}

impl Default for ZoneRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    // ── Portal surface structural validation (RFC 0013 §7.2; hud-tc153) ──────

    fn portal_part(kind: PortalPartKind) -> PortalPart {
        PortalPart {
            kind,
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            node: None,
        }
    }

    #[test]
    fn portal_surface_validate_accepts_all_eight_parts() {
        let surface = PortalSurface {
            parts: PortalPartKind::ALL
                .iter()
                .copied()
                .map(portal_part)
                .collect(),
            ..Default::default()
        };
        assert_eq!(surface.parts.len(), PORTAL_MAX_PARTS);
        assert!(surface.validate_structure().is_ok());
    }

    #[test]
    fn portal_surface_validate_rejects_duplicate_kind() {
        let surface = PortalSurface {
            parts: vec![
                portal_part(PortalPartKind::Transcript),
                portal_part(PortalPartKind::Transcript),
            ],
            ..Default::default()
        };
        assert!(
            surface
                .validate_structure()
                .unwrap_err()
                .contains("duplicate")
        );
    }

    #[test]
    fn portal_surface_validate_rejects_oversized_identity() {
        let surface = PortalSurface {
            identity: PortalIdentity {
                session_id: "x".repeat(PORTAL_SESSION_ID_MAX_BYTES + 1),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(
            surface
                .validate_structure()
                .unwrap_err()
                .contains("session_id")
        );

        let surface = PortalSurface {
            identity: PortalIdentity {
                display_name: "y".repeat(PORTAL_DISPLAY_NAME_MAX_BYTES + 1),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(
            surface
                .validate_structure()
                .unwrap_err()
                .contains("display_name")
        );
    }

    #[test]
    fn portal_surface_validate_rejects_non_finite_bounds() {
        let surface = PortalSurface {
            parts: vec![PortalPart {
                kind: PortalPartKind::Frame,
                bounds: Rect::new(f32::NAN, 0.0, 10.0, 10.0),
                node: None,
            }],
            ..Default::default()
        };
        assert!(
            surface
                .validate_structure()
                .unwrap_err()
                .contains("non-finite")
        );
    }

    #[test]
    fn portal_surface_validate_rejects_negative_extent_bounds() {
        for bounds in [
            Rect::new(0.0, 0.0, -10.0, 10.0),
            Rect::new(0.0, 0.0, 10.0, -10.0),
        ] {
            let surface = PortalSurface {
                parts: vec![PortalPart {
                    kind: PortalPartKind::Frame,
                    bounds,
                    node: None,
                }],
                ..Default::default()
            };
            assert!(
                surface
                    .validate_structure()
                    .unwrap_err()
                    .contains("negative bounds extent"),
                "negative-extent bounds {bounds:?} must be rejected"
            );
        }
    }

    #[test]
    fn portal_part_kind_text_bearing_classification() {
        // Geometry-only parts are NOT text-bearing (readability technique None).
        for k in [
            PortalPartKind::Divider,
            PortalPartKind::CaptureBackstop,
            PortalPartKind::GestureShield,
        ] {
            assert!(!k.is_text_bearing(), "{k:?} must be geometry-only");
        }
        for k in [
            PortalPartKind::Frame,
            PortalPartKind::Header,
            PortalPartKind::Composer,
            PortalPartKind::Transcript,
            PortalPartKind::CollapsedCard,
        ] {
            assert!(k.is_text_bearing(), "{k:?} must be text-bearing");
        }
    }

    // ── SceneId size invariant ────────────────────────────────────────────────

    #[test]
    fn scene_id_size_is_16_bytes() {
        assert_eq!(size_of::<SceneId>(), 16, "SceneId must be exactly 16 bytes");
    }

    // ── SceneId null sentinel ─────────────────────────────────────────────────

    #[test]
    fn scene_id_null_is_all_zeros() {
        let null = SceneId::null();
        assert!(
            null.is_null(),
            "SceneId::null() must report is_null() == true"
        );
        assert_eq!(
            null.to_bytes_le(),
            [0u8; 16],
            "null SceneId must serialize to 16 zero bytes"
        );
    }

    #[test]
    fn scene_id_new_is_never_null() {
        let id = SceneId::new();
        assert!(!id.is_null(), "freshly-generated SceneId must not be null");
    }

    #[test]
    fn scene_id_nil_aliases_null() {
        assert_eq!(SceneId::nil(), SceneId::null());
        assert!(SceneId::nil().is_nil());
    }

    // ── SceneId byte round-trip ───────────────────────────────────────────────

    #[test]
    fn scene_id_bytes_le_round_trip() {
        let id = SceneId::new();
        let bytes = id.to_bytes_le();
        let restored = SceneId::from_bytes_le(&bytes).expect("must decode 16 bytes");
        assert_eq!(id, restored, "SceneId bytes LE round-trip must be lossless");
    }

    #[test]
    fn scene_id_from_bytes_le_rejects_wrong_length() {
        assert!(SceneId::from_bytes_le(&[0u8; 15]).is_none());
        assert!(SceneId::from_bytes_le(&[0u8; 17]).is_none());
        assert!(SceneId::from_bytes_le(&[]).is_none());
    }

    #[test]
    fn scene_id_null_round_trips_via_bytes() {
        let null = SceneId::null();
        let bytes = null.to_bytes_le();
        let restored = SceneId::from_bytes_le(&bytes).unwrap();
        assert!(restored.is_null());
    }

    // ── SceneId lexicographic / monotonicity ─────────────────────────────────

    #[test]
    fn scene_id_monotonic_small_batch() {
        // Generate a small batch synchronously and verify they're non-decreasing.
        // (A full 10,000-ID property test is in the proptest suite below.)
        let ids: Vec<SceneId> = (0..64).map(|_| SceneId::new()).collect();
        for w in ids.windows(2) {
            assert!(
                w[0] <= w[1],
                "SceneId sequence must be non-decreasing: {:?} > {:?}",
                w[0],
                w[1]
            );
        }
    }

    // ── ResourceId size invariant ─────────────────────────────────────────────

    #[test]
    fn resource_id_size_is_32_bytes() {
        assert_eq!(
            size_of::<ResourceId>(),
            32,
            "ResourceId must be exactly 32 bytes"
        );
    }

    // ── ResourceId content deduplication ─────────────────────────────────────

    #[test]
    fn resource_id_same_content_same_id() {
        let data = b"hello world";
        let id1 = ResourceId::of(data);
        let id2 = ResourceId::of(data);
        assert_eq!(
            id1, id2,
            "identical content must produce the same ResourceId"
        );
    }

    #[test]
    fn resource_id_different_content_different_id() {
        let id1 = ResourceId::of(b"foo");
        let id2 = ResourceId::of(b"bar");
        assert_ne!(
            id1, id2,
            "different content must produce different ResourceIds"
        );
    }

    #[test]
    fn resource_id_empty_content() {
        let id = ResourceId::of(b"");
        assert_eq!(id.as_bytes().len(), 32);
    }

    // ── ResourceId byte round-trip ────────────────────────────────────────────

    #[test]
    fn resource_id_from_bytes_round_trip() {
        let id = ResourceId::of(b"round-trip test payload");
        let bytes = *id.as_bytes();
        let restored = ResourceId::from_bytes(bytes);
        assert_eq!(id, restored);
    }

    #[test]
    fn resource_id_from_slice_round_trip() {
        let id = ResourceId::of(b"slice round-trip");
        let restored = ResourceId::from_slice(id.as_bytes()).expect("must accept 32-byte slice");
        assert_eq!(id, restored);
    }

    #[test]
    fn resource_id_from_slice_rejects_wrong_length() {
        assert!(ResourceId::from_slice(&[0u8; 31]).is_none());
        assert!(ResourceId::from_slice(&[0u8; 33]).is_none());
        assert!(ResourceId::from_slice(&[]).is_none());
    }

    // ── ResourceId display / hex is debug-only ────────────────────────────────

    #[test]
    fn resource_id_to_hex_is_64_chars() {
        let id = ResourceId::of(b"hex display test");
        let hex = id.to_hex();
        assert_eq!(hex.len(), 64, "hex of 32-byte hash must be 64 chars");
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "must be valid hex"
        );
    }

    // ── ResourceId::from_hex round-trip ──────────────────────────────────────

    /// `from_hex(to_hex(id)) == id` for any ResourceId.
    #[test]
    fn resource_id_from_hex_round_trip() {
        let id = ResourceId::of(b"from_hex round-trip test");
        let hex = id.to_hex();
        let restored = ResourceId::from_hex(&hex).expect("from_hex must succeed on valid hex");
        assert_eq!(id, restored, "from_hex(to_hex(id)) must equal id");
    }

    /// `from_hex` rejects a non-64-character string.
    #[test]
    fn resource_id_from_hex_rejects_short_string() {
        assert!(ResourceId::from_hex("abc").is_none());
        assert!(ResourceId::from_hex("").is_none());
    }

    /// `from_hex` rejects a 64-character string with non-hex characters
    /// (e.g. a human-readable name like `"shield"`).
    #[test]
    fn resource_id_from_hex_rejects_non_hex_string() {
        // 64 chars but not valid hex (contains 'g' and 'z').
        let bad = "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg";
        assert_eq!(bad.len(), 64);
        assert!(ResourceId::from_hex(bad).is_none());
    }

    /// `from_hex` rejects a human-readable icon name like `"shield"`.
    #[test]
    fn resource_id_from_hex_rejects_human_readable_name() {
        assert!(ResourceId::from_hex("shield").is_none());
        assert!(ResourceId::from_hex("update").is_none());
        assert!(ResourceId::from_hex("").is_none());
    }

    // ── Layer 0 identity invariant check helper ───────────────────────────────

    /// Validates the core Layer 0 identity invariants for `SceneId` and `ResourceId`.
    /// This function mirrors what `assert_layer0_invariants` checks at the graph level
    /// but focuses on the type-level contracts.
    pub fn assert_identity_invariants() -> Vec<String> {
        let mut violations = Vec::new();

        if size_of::<SceneId>() != 16 {
            violations.push(format!("SceneId size {} != 16", size_of::<SceneId>()));
        }
        if size_of::<ResourceId>() != 32 {
            violations.push(format!("ResourceId size {} != 32", size_of::<ResourceId>()));
        }
        if !SceneId::null().is_null() {
            violations.push("SceneId::null() does not report is_null()".into());
        }
        if SceneId::new().is_null() {
            violations.push("freshly-generated SceneId reports is_null()".into());
        }
        let id = ResourceId::of(b"test");
        if ResourceId::of(b"test") != id {
            violations.push("ResourceId deduplication failed".into());
        }

        violations
    }

    #[test]
    fn layer0_identity_invariants_pass() {
        let violations = assert_identity_invariants();
        assert!(
            violations.is_empty(),
            "Layer 0 identity violations: {violations:?}"
        );
    }

    #[test]
    fn widget_instance_missing_id_defaults_to_null() {
        let tab_id = SceneId::new();
        let mut value = serde_json::to_value(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "gauge".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: "main".to_string(),
            current_params: HashMap::new(),
        })
        .expect("serialize widget instance");
        value
            .as_object_mut()
            .expect("widget instance encodes as object")
            .remove("id");

        let restored: WidgetInstance =
            serde_json::from_value(value).expect("deserialize widget instance without id");
        assert!(
            restored.id.is_null(),
            "missing id must default to SceneId::null()"
        );
    }

    #[test]
    fn geometry_override_resolution_order_is_user_then_agent_then_config_then_default() {
        let user = GeometryPolicy::Relative {
            x_pct: 0.10,
            y_pct: 0.10,
            width_pct: 0.20,
            height_pct: 0.20,
        };
        let agent = GeometryPolicy::Relative {
            x_pct: 0.30,
            y_pct: 0.30,
            width_pct: 0.20,
            height_pct: 0.20,
        };
        let config = GeometryPolicy::Relative {
            x_pct: 0.50,
            y_pct: 0.50,
            width_pct: 0.20,
            height_pct: 0.20,
        };
        let default = GeometryPolicy::Relative {
            x_pct: 0.70,
            y_pct: 0.70,
            width_pct: 0.20,
            height_pct: 0.20,
        };

        assert_eq!(
            resolve_geometry_override_chain(Some(user), Some(agent), Some(config), Some(default)),
            Some(user)
        );
        assert_eq!(
            resolve_geometry_override_chain(None, Some(agent), Some(config), Some(default)),
            Some(agent)
        );
        assert_eq!(
            resolve_geometry_override_chain(None, None, Some(config), Some(default)),
            Some(config)
        );
        assert_eq!(
            resolve_geometry_override_chain(None, None, None, Some(default)),
            Some(default)
        );
        assert_eq!(
            resolve_geometry_override_chain(None, None, None, None),
            None
        );
    }

    #[test]
    fn rect_to_relative_geometry_policy_converts_expected_percentages() {
        let rect = Rect::new(192.0, 108.0, 960.0, 540.0);
        let policy = rect_to_relative_geometry_policy(rect, 1920.0, 1080.0);
        assert_eq!(
            policy,
            GeometryPolicy::Relative {
                x_pct: 0.10,
                y_pct: 0.10,
                width_pct: 0.50,
                height_pct: 0.50,
            }
        );
    }

    #[test]
    fn widget_registry_resolves_user_override_before_instance_and_default() {
        let mut registry = WidgetRegistry::new();
        let tab_id = SceneId::new();
        let default_policy = GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.3,
            height_pct: 0.3,
        };
        registry.register_definition(WidgetDefinition {
            id: "gauge".to_string(),
            name: "Gauge".to_string(),
            description: "test".to_string(),
            parameter_schema: vec![],
            layers: vec![],
            default_geometry_policy: default_policy,
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: ContentionPolicy::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: false,
            hover_behavior: None,
        });

        let config_override = GeometryPolicy::Relative {
            x_pct: 0.2,
            y_pct: 0.2,
            width_pct: 0.4,
            height_pct: 0.4,
        };
        registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "gauge".to_string(),
            tab_id,
            geometry_override: Some(config_override),
            contention_override: None,
            instance_name: "gauge-main".to_string(),
            current_params: HashMap::new(),
        });

        let user_override = GeometryPolicy::Relative {
            x_pct: 0.6,
            y_pct: 0.1,
            width_pct: 0.2,
            height_pct: 0.2,
        };
        assert_eq!(
            registry.resolve_geometry_policy_for_instance("gauge-main", Some(&user_override)),
            Some(user_override)
        );
        assert_eq!(
            registry.resolve_geometry_policy_for_instance("gauge-main", None),
            Some(config_override)
        );
    }

    #[test]
    fn zone_registry_resolves_user_override_before_config_and_default() {
        let mut registry = ZoneRegistry::new();
        let default_policy = GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.8,
            width_pct: 1.0,
            height_pct: 0.2,
        };
        registry.register(ZoneDefinition {
            id: SceneId::new(),
            name: "subtitle".to_string(),
            description: "subtitle".to_string(),
            geometry_policy: default_policy,
            accepted_media_types: vec![ZoneMediaType::StreamText],
            rendering_policy: RenderingPolicy::default(),
            contention_policy: ContentionPolicy::LatestWins,
            max_publishers: 1,
            auto_clear_ms: None,
            ephemeral: false,
            layer_attachment: LayerAttachment::Content,
        });

        let config_override = GeometryPolicy::Relative {
            x_pct: 0.1,
            y_pct: 0.7,
            width_pct: 0.8,
            height_pct: 0.2,
        };
        let user_override = GeometryPolicy::Relative {
            x_pct: 0.2,
            y_pct: 0.6,
            width_pct: 0.6,
            height_pct: 0.2,
        };
        assert_eq!(
            registry.resolve_geometry_policy_for_zone(
                "subtitle",
                Some(&user_override),
                Some(&config_override)
            ),
            Some(user_override)
        );
        assert_eq!(
            registry.resolve_geometry_policy_for_zone("subtitle", None, Some(&config_override)),
            Some(config_override)
        );
        assert_eq!(
            registry.resolve_geometry_policy_for_zone("subtitle", None, None),
            Some(default_policy)
        );
    }
}

// ─── Property tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    /// Generates 10,000 SceneIds and asserts they are monotonically non-decreasing.
    ///
    /// UUIDv7 guarantees creation-time ordering via a monotonic counter within the
    /// same millisecond, so lexicographic sort == chronological sort.
    #[test]
    fn scene_id_monotonic_10k() {
        let ids: Vec<SceneId> = (0..10_000).map(|_| SceneId::new()).collect();
        for w in ids.windows(2) {
            assert!(
                w[0] <= w[1],
                "SceneId not monotonically non-decreasing: {:?} > {:?}",
                w[0],
                w[1]
            );
        }
    }

    proptest! {
        /// Verifies that any 16-byte input round-trips through SceneId bytes LE encoding.
        #[test]
        fn scene_id_bytes_le_roundtrip_arb(raw in proptest::array::uniform16(0u8..)) {
            // from_bytes_le -> to_bytes_le must be identity
            let id = SceneId::from_bytes_le(&raw).expect("uniform16 is always 16 bytes");
            prop_assert_eq!(id.to_bytes_le(), raw);
        }

        /// Verifies that any 32-byte slice round-trips through ResourceId.
        #[test]
        fn resource_id_bytes_roundtrip_arb(raw in proptest::array::uniform32(0u8..)) {
            let id = ResourceId::from_bytes(raw);
            prop_assert_eq!(*id.as_bytes(), raw);
        }

        /// Verifies BLAKE3 determinism: same input always produces the same ResourceId.
        #[test]
        fn resource_id_deterministic(data in proptest::collection::vec(0u8.., 0..1024)) {
            let id1 = ResourceId::of(&data);
            let id2 = ResourceId::of(&data);
            prop_assert_eq!(id1, id2);
        }

        /// Verifies that distinct inputs produce distinct ResourceIds (collision resistance).
        #[test]
        fn resource_id_distinct_inputs_distinct_ids(
            a in proptest::collection::vec(0u8.., 1..512),
            b in proptest::collection::vec(0u8.., 1..512),
        ) {
            // Only assert when inputs differ
            if a != b {
                let id_a = ResourceId::of(&a);
                let id_b = ResourceId::of(&b);
                prop_assert_ne!(id_a, id_b, "distinct content must yield distinct ResourceIds");
            }
        }
    }
}
