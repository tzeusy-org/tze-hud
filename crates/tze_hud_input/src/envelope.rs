//! Rust-native input event envelope types for the batching/coalescing pipeline.
//!
//! These mirror the protobuf definitions in `events.proto` but live in the input
//! crate so that batching and coalescing logic has no dependency on the protocol
//! crate or generated protobuf code. The runtime converts these to wire types
//! before delivery.
//!
//! # Transactional vs ephemeral events
//!
//! Per spec.md §8.5 (RFC 0004 §8.5):
//! - **Transactional**: down, up, click, key, focus, capture, command.
//!   Never dropped or coalesced. Guaranteed zero-loss delivery.
//! - **Ephemeral realtime**: PointerMove, PointerEnter, PointerLeave, hover
//!   state changes, ScrollOffsetChanged.
//!   May be coalesced or dropped under backpressure.

use tze_hud_scene::{MonoUs, SceneId};

// ─── Per-event payload types ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct PointerMoveData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub interaction_id: String,
    pub timestamp_mono_us: MonoUs,
    pub device_id: String,
    pub local_x: f32,
    pub local_y: f32,
    pub display_x: f32,
    pub display_y: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PointerEnterData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub interaction_id: String,
    pub timestamp_mono_us: MonoUs,
    pub device_id: String,
    pub local_x: f32,
    pub local_y: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PointerLeaveData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub interaction_id: String,
    pub timestamp_mono_us: MonoUs,
    pub device_id: String,
}

#[derive(Clone, Debug)]
pub struct PointerDownData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub interaction_id: String,
    pub timestamp_mono_us: MonoUs,
    pub device_id: String,
    pub local_x: f32,
    pub local_y: f32,
    pub display_x: f32,
    pub display_y: f32,
    pub button: u32,
}

#[derive(Clone, Debug)]
pub struct PointerUpData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub interaction_id: String,
    pub timestamp_mono_us: MonoUs,
    pub device_id: String,
    pub local_x: f32,
    pub local_y: f32,
    pub display_x: f32,
    pub display_y: f32,
    pub button: u32,
}

#[derive(Clone, Debug)]
pub struct ClickData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub interaction_id: String,
    pub timestamp_mono_us: MonoUs,
    pub device_id: String,
    pub local_x: f32,
    pub local_y: f32,
    pub button: u32,
}

#[derive(Clone, Debug)]
pub struct PointerCancelData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub interaction_id: String,
    pub timestamp_mono_us: MonoUs,
    pub device_id: String,
}

#[derive(Clone, Debug)]
pub struct KeyDownData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub timestamp_mono_us: MonoUs,
    pub key_code: String,
    pub key: String,
    pub repeat: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

#[derive(Clone, Debug)]
pub struct KeyUpData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub timestamp_mono_us: MonoUs,
    pub key_code: String,
    pub key: String,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

#[derive(Clone, Debug)]
pub struct CharacterData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub timestamp_mono_us: MonoUs,
    pub character: String,
}

#[derive(Clone, Debug)]
pub struct FocusGainedData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub timestamp_mono_us: MonoUs,
    pub source: FocusSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusSource {
    Unspecified = 0,
    Click = 1,
    TabKey = 2,
    Programmatic = 3,
    CommandInput = 4,
}

#[derive(Clone, Debug)]
pub struct FocusLostData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub timestamp_mono_us: MonoUs,
    pub reason: FocusLostReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusLostReason {
    Unspecified = 0,
    ClickElsewhere = 1,
    TabKey = 2,
    Programmatic = 3,
    TileDestroyed = 4,
    TabSwitched = 5,
    LeaseRevoked = 6,
    AgentDisconnected = 7,
    CommandInput = 8,
}

#[derive(Clone, Debug)]
pub struct CaptureReleasedData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub timestamp_mono_us: MonoUs,
    pub device_id: String,
    pub reason: CaptureReleasedReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureReleasedReason {
    Unspecified = 0,
    AgentReleased = 1,
    PointerUp = 2,
    RuntimeRevoked = 3,
    LeaseRevoked = 4,
}

#[derive(Clone, Debug)]
pub struct ScrollOffsetChangedData {
    pub tile_id: SceneId,
    pub timestamp_mono_us: MonoUs,
    pub offset_x: f32,
    pub offset_y: f32,
}

#[derive(Clone, Debug)]
pub struct CommandInputData {
    pub tile_id: SceneId,
    pub node_id: SceneId,
    pub interaction_id: String,
    pub timestamp_mono_us: MonoUs,
    pub device_id: String,
    pub action: CommandAction,
    pub source: CommandSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandAction {
    Unspecified = 0,
    NavigateNext = 1,
    NavigatePrev = 2,
    Activate = 3,
    Cancel = 4,
    Context = 5,
    ScrollUp = 6,
    ScrollDown = 7,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandSource {
    Unspecified = 0,
    Keyboard = 1,
    Dpad = 2,
    Voice = 3,
    RemoteClicker = 4,
    RotaryDial = 5,
    Programmatic = 6,
}

// ─── InputEnvelope (19 implemented variants; 3 proto fields reserved) ────────

/// A single input event, discriminated by variant.
///
/// Mirrors the `InputEnvelope` protobuf oneof from `events.proto`.
/// Ordering of variants matches field numbers in the proto file.
/// Fields 7 (DoubleClick), 8 (ContextMenu), and 19 are reserved in the proto
/// but not yet implemented as Rust variants in this batching pipeline.
/// The spec references 22 total proto fields including those reserved.
#[derive(Clone, Debug)]
pub enum InputEnvelope {
    // field 1 — transactional
    PointerDown(PointerDownData),
    // field 2 — transactional
    PointerUp(PointerUpData),
    // field 3 — ephemeral realtime
    PointerMove(PointerMoveData),
    // field 4 — ephemeral realtime
    PointerEnter(PointerEnterData),
    // field 5 — ephemeral realtime
    PointerLeave(PointerLeaveData),
    // field 6 — transactional
    Click(ClickData),
    // field 9 — transactional
    PointerCancel(PointerCancelData),
    // field 10 — transactional
    KeyDown(KeyDownData),
    // field 11 — transactional
    KeyUp(KeyUpData),
    // field 12 — transactional
    Character(CharacterData),
    // field 13 — transactional
    FocusGained(FocusGainedData),
    // field 14 — transactional
    FocusLost(FocusLostData),
    // field 20 — transactional
    CaptureReleased(CaptureReleasedData),
    // field 21 — ephemeral realtime (coalesced per tile)
    ScrollOffsetChanged(ScrollOffsetChangedData),
    // field 22 — transactional
    CommandInput(CommandInputData),
}

impl InputEnvelope {
    /// Returns `true` if this event must never be dropped or coalesced.
    ///
    /// Transactional events: down, up, click, cancel, key, focus, capture, command.
    /// Ephemeral events: move, enter, leave, scroll_offset_changed.
    pub fn is_transactional(&self) -> bool {
        matches!(
            self,
            InputEnvelope::PointerDown(_)
                | InputEnvelope::PointerUp(_)
                | InputEnvelope::Click(_)
                | InputEnvelope::PointerCancel(_)
                | InputEnvelope::KeyDown(_)
                | InputEnvelope::KeyUp(_)
                | InputEnvelope::Character(_)
                | InputEnvelope::FocusGained(_)
                | InputEnvelope::FocusLost(_)
                | InputEnvelope::CaptureReleased(_)
                | InputEnvelope::CommandInput(_)
        )
    }

    /// Extract the hardware timestamp (monotonic microseconds) from any event variant.
    pub fn timestamp_mono_us(&self) -> MonoUs {
        match self {
            InputEnvelope::PointerDown(d) => d.timestamp_mono_us,
            InputEnvelope::PointerUp(d) => d.timestamp_mono_us,
            InputEnvelope::PointerMove(d) => d.timestamp_mono_us,
            InputEnvelope::PointerEnter(d) => d.timestamp_mono_us,
            InputEnvelope::PointerLeave(d) => d.timestamp_mono_us,
            InputEnvelope::Click(d) => d.timestamp_mono_us,
            InputEnvelope::PointerCancel(d) => d.timestamp_mono_us,
            InputEnvelope::KeyDown(d) => d.timestamp_mono_us,
            InputEnvelope::KeyUp(d) => d.timestamp_mono_us,
            InputEnvelope::Character(d) => d.timestamp_mono_us,
            InputEnvelope::FocusGained(d) => d.timestamp_mono_us,
            InputEnvelope::FocusLost(d) => d.timestamp_mono_us,
            InputEnvelope::CaptureReleased(d) => d.timestamp_mono_us,
            InputEnvelope::ScrollOffsetChanged(d) => d.timestamp_mono_us,
            InputEnvelope::CommandInput(d) => d.timestamp_mono_us,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::{MonoUs, SceneId};

    fn null_id() -> SceneId {
        SceneId::null()
    }

    #[test]
    fn test_transactional_classification() {
        let down = InputEnvelope::PointerDown(PointerDownData {
            tile_id: null_id(),
            node_id: null_id(),
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs::NOT_SET,
            device_id: String::new(),
            local_x: 0.0,
            local_y: 0.0,
            display_x: 0.0,
            display_y: 0.0,
            button: 0,
        });
        assert!(down.is_transactional());

        let mv = InputEnvelope::PointerMove(PointerMoveData {
            tile_id: null_id(),
            node_id: null_id(),
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs::NOT_SET,
            device_id: String::new(),
            local_x: 0.0,
            local_y: 0.0,
            display_x: 0.0,
            display_y: 0.0,
        });
        assert!(!mv.is_transactional());

        let scroll = InputEnvelope::ScrollOffsetChanged(ScrollOffsetChangedData {
            tile_id: null_id(),
            timestamp_mono_us: MonoUs::NOT_SET,
            offset_x: 0.0,
            offset_y: 0.0,
        });
        assert!(!scroll.is_transactional());

        let cmd = InputEnvelope::CommandInput(CommandInputData {
            tile_id: null_id(),
            node_id: null_id(),
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs::NOT_SET,
            device_id: String::new(),
            action: CommandAction::Activate,
            source: CommandSource::Keyboard,
        });
        assert!(cmd.is_transactional());
    }

    #[test]
    fn test_timestamp_extraction() {
        let mv = InputEnvelope::PointerMove(PointerMoveData {
            tile_id: null_id(),
            node_id: null_id(),
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(42_000),
            device_id: String::new(),
            local_x: 0.0,
            local_y: 0.0,
            display_x: 0.0,
            display_y: 0.0,
        });
        assert_eq!(mv.timestamp_mono_us(), MonoUs(42_000));
    }
}
