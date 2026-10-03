//! Rich pointer event types for the dispatch pipeline.
//!
//! Implements spec §Requirement: Pointer Event Types (lines 278-280) and the
//! protobuf schema concepts from §Requirement: Protobuf Schema for Input Events
//! (lines 367-369).
//!
//! All events carry:
//! - `tile_id`, `node_id` as `SceneId` (16-byte little-endian UUIDv7)
//! - `device_id` identifying the input device
//! - node-local and display-space coordinates
//! - modifier key state
//! - `timestamp_mono_us`: OS hardware event timestamp in the monotonic domain

use serde::{Deserialize, Serialize};
use tze_hud_scene::{MonoUs, SceneId};

// ─── Modifier keys ────────────────────────────────────────────────────────────

/// Modifier key state at the time of an input event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// Command / Windows / Meta key.
    pub meta: bool,
}

impl Modifiers {
    /// No modifier keys held.
    pub const NONE: Modifiers = Modifiers {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
    };
}

// ─── Pointer button ──────────────────────────────────────────────────────────

/// Which pointer button triggered the event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PointerButton {
    #[default]
    Primary, // Left mouse button / main touch contact
    Secondary, // Right mouse button / two-finger tap
    Middle,    // Middle mouse button / wheel press
    Other(u8), // X1, X2, additional buttons
}

// ─── Common pointer fields ────────────────────────────────────────────────────

/// Fields shared by all pointer events.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointerFields {
    /// The tile that owns the hit node.
    pub tile_id: SceneId,
    /// The specific HitRegionNode that was hit.
    /// Use `SceneId::null()` when the event targets a tile but no HitRegionNode
    /// (i.e., the dispatch resulted in a `TileHit`). Callers should check
    /// `node_id.is_null()` before treating this as a node reference.
    pub node_id: SceneId,
    /// The agent-defined interaction_id of the hit node.
    pub interaction_id: String,
    /// Opaque device identifier (OS-assigned).
    pub device_id: u64,
    /// Pointer position in tile-local coordinates (display minus tile origin).
    ///
    /// Note: v1 delivers tile-local coordinates, not true node-local coordinates.
    /// Agents that need coordinates relative to a specific node's bounds must
    /// subtract the node's `HitRegionNode.bounds` origin themselves.
    pub local_x: f32,
    pub local_y: f32,
    /// Pointer position in display-space coordinates.
    pub display_x: f32,
    pub display_y: f32,
    /// Modifier keys held at the time of the event.
    pub modifiers: Modifiers,
    /// OS hardware event timestamp, monotonic domain, microseconds.
    pub timestamp_mono_us: MonoUs,
}

// ─── Per-event types ─────────────────────────────────────────────────────────

/// Pointer button pressed (spec line 279).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointerDownEvent {
    pub fields: PointerFields,
    pub button: PointerButton,
}

/// Pointer button released (spec line 279).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointerUpEvent {
    pub fields: PointerFields,
    pub button: PointerButton,
}

/// Pointer moved while over the node (spec line 279).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointerMoveEvent {
    pub fields: PointerFields,
}

/// Pointer entered the node's bounds (spec line 279).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointerEnterEvent {
    pub fields: PointerFields,
}

/// Pointer left the node's bounds (spec line 279).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointerLeaveEvent {
    pub fields: PointerFields,
}

/// Press + release on the same node (spec line 279).
///
/// Maps to primary button (left-click) interaction. Dispatched only after
/// PointerUpEvent, in the same event batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClickEvent {
    pub fields: PointerFields,
    pub button: PointerButton,
}

/// Two clicks within 300ms on the same node (spec line 279, v1-reserved tap
/// pipeline, but DoubleClick recognition ships per V1 Gesture Fallback).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DoubleClickEvent {
    pub fields: PointerFields,
    pub button: PointerButton,
}

/// Right-click context menu request (spec lines 433-435).
///
/// Produced by the event preprocessor directly from right-click, bypassing
/// the gesture recognizer pipeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextMenuEvent {
    pub fields: PointerFields,
}

/// In-progress pointer interaction cancelled (spec line 279).
///
/// Terminal event for an interaction. Agents MUST treat this as the end
/// of the interaction sequence — no PointerUp follows a PointerCancel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointerCancelEvent {
    pub fields: PointerFields,
    /// Reason the interaction was cancelled.
    pub reason: CancelReason,
}

/// Why a pointer interaction was cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CancelReason {
    /// Runtime stole capture for a system event (Alt+Tab, notification, etc).
    RuntimeRevoked,
    /// Lease for the owning tile was revoked.
    LeaseRevoked,
    /// Agent disconnected while interaction was in progress.
    AgentDisconnected,
    /// Tab switched while interaction was in progress.
    TabSwitched,
    /// Unspecified system cancellation.
    Other,
}

// ─── Raw input event (OS-facing) ─────────────────────────────────────────────

/// Raw input event arriving from the OS / winit event loop.
///
/// This is the Stage 1 input — before hit-test or any agent routing.
/// `timestamp_mono_us` is attached by Stage 1 (Input Drain).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RawPointerEvent {
    /// Display-space X coordinate.
    pub x: f32,
    /// Display-space Y coordinate.
    pub y: f32,
    /// What kind of event this is.
    pub kind: RawPointerEventKind,
    /// Which button, if applicable.
    pub button: Option<PointerButton>,
    /// Opaque device identifier from the OS.
    pub device_id: u64,
    /// OS hardware event timestamp in the monotonic domain (microseconds).
    /// Attached by Stage 1 Input Drain.
    pub timestamp_mono_us: MonoUs,
    /// Modifier keys held at the time.
    pub modifiers: Modifiers,
}

/// The kind of raw pointer event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawPointerEventKind {
    Move,
    Down,
    Up,
    /// Right-click (preprocessor produces ContextMenuEvent instead of passing
    /// through the gesture pipeline — spec §3.2, §3.3).
    RightClick,
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_default_is_none() {
        let m = Modifiers::default();
        assert!(!m.shift && !m.ctrl && !m.alt && !m.meta);
        assert_eq!(m, Modifiers::NONE);
    }

    #[test]
    fn pointer_fields_roundtrip_serde() {
        let f = PointerFields {
            tile_id: SceneId::new(),
            node_id: SceneId::new(),
            interaction_id: "submit-btn".to_string(),
            device_id: 42,
            local_x: 10.0,
            local_y: 20.0,
            display_x: 110.0,
            display_y: 120.0,
            modifiers: Modifiers {
                shift: true,
                ctrl: false,
                alt: false,
                meta: false,
            },
            timestamp_mono_us: MonoUs(1_234_567),
        };
        let json = serde_json::to_string(&f).unwrap();
        let decoded: PointerFields = serde_json::from_str(&json).unwrap();
        assert_eq!(f, decoded);
    }

    #[test]
    fn context_menu_event_carries_required_fields() {
        let tile_id = SceneId::new();
        let node_id = SceneId::new();
        let evt = ContextMenuEvent {
            fields: PointerFields {
                tile_id,
                node_id,
                interaction_id: "submit-button".to_string(),
                device_id: 1,
                local_x: 5.0,
                local_y: 5.0,
                display_x: 105.0,
                display_y: 105.0,
                modifiers: Modifiers::NONE,
                timestamp_mono_us: MonoUs(999),
            },
        };
        assert_eq!(evt.fields.tile_id, tile_id);
        assert_eq!(evt.fields.node_id, node_id);
        assert_eq!(evt.fields.interaction_id, "submit-button");
    }
}
