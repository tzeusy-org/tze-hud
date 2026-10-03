//! Event coalescing under backpressure (spec.md §8.5 / RFC 0004 §8.5).
//!
//! Rules:
//! - `PointerMoveEvent`: coalesced to the **latest** position for the same node.
//! - Hover state changes (`PointerEnter`/`PointerLeave`): coalesced to **net state**
//!   (a leave followed by an enter on the same node collapses to enter; vice versa).
//! - `ScrollOffsetChangedEvent`: coalesced per tile to the **latest** offset.
//! - All **transactional** events are never coalesced or dropped.
//!
//! The coalescer is applied to a candidate list of events before they are pushed
//! into the agent's `AgentEventQueue`. It is called when the queue is already at
//! capacity (i.e., under backpressure) and a new non-transactional event arrives.
//!
//! Usage pattern:
//! 1. Under backpressure, call `EventCoalescer::coalesce_move` / `coalesce_scroll`
//!    / `coalesce_hover` instead of enqueueing naively.
//! 2. The coalescer replaces the relevant existing event in-place when possible.
//! 3. Returns `CoalesceResult` indicating whether the incoming event was merged
//!    into an existing slot (no need to enqueue) or should be enqueued normally.

use crate::envelope::{
    InputEnvelope, PointerEnterData, PointerLeaveData, PointerMoveData, ScrollOffsetChangedData,
};
use std::collections::HashMap;
use tze_hud_scene::SceneId;

/// Outcome from a coalescing attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum CoalesceResult {
    /// The incoming event was merged into an existing slot in the queue.
    /// Callers do NOT need to enqueue the event again.
    Merged,
    /// No matching slot found; the event should be enqueued normally.
    NotMerged,
}

/// Applies in-place coalescing to an existing event queue.
///
/// All methods operate on a `&mut Vec<InputEnvelope>` — the live queue.
/// The queue may contain a mix of transactional and non-transactional events;
/// only non-transactional events of the matching type are candidates.
pub struct EventCoalescer;

impl EventCoalescer {
    /// Attempt to coalesce an incoming `PointerMove` into an existing move for
    /// the same `(tile_id, node_id)` pair already in the queue.
    ///
    /// If found, the existing entry is **replaced** with the incoming event
    /// (latest position wins). Returns `Merged` if replaced, `NotMerged` otherwise.
    pub fn coalesce_move(queue: &mut [InputEnvelope], incoming: PointerMoveData) -> CoalesceResult {
        for slot in queue.iter_mut().rev() {
            if let InputEnvelope::PointerMove(existing) = slot
                && existing.tile_id == incoming.tile_id
                && existing.node_id == incoming.node_id
            {
                *existing = incoming;
                return CoalesceResult::Merged;
            }
        }
        CoalesceResult::NotMerged
    }

    /// Attempt to coalesce an incoming `ScrollOffsetChanged` into an existing
    /// scroll event for the same `tile_id` in the queue.
    ///
    /// Latest offset wins. Returns `Merged` if replaced, `NotMerged` otherwise.
    pub fn coalesce_scroll(
        queue: &mut [InputEnvelope],
        incoming: ScrollOffsetChangedData,
    ) -> CoalesceResult {
        for slot in queue.iter_mut().rev() {
            if let InputEnvelope::ScrollOffsetChanged(existing) = slot
                && existing.tile_id == incoming.tile_id
            {
                *existing = incoming;
                return CoalesceResult::Merged;
            }
        }
        CoalesceResult::NotMerged
    }

    /// Coalesce hover state transitions (PointerEnter / PointerLeave) for a node.
    ///
    /// Strategy: find the most recent `PointerEnter` or `PointerLeave` for the
    /// same `(tile_id, node_id)` pair in the queue.
    ///
    /// - If found and it is the **same** variant as the incoming event: merge
    ///   (update timestamp; the net state is identical). Returns `Merged`.
    /// - If found and it is the **opposite** variant: the net state is the
    ///   incoming event's state. Replace the existing event with the incoming
    ///   event. Returns `Merged`.
    /// - If not found: Returns `NotMerged` (caller enqueues normally).
    pub fn coalesce_enter(
        queue: &mut [InputEnvelope],
        incoming: PointerEnterData,
    ) -> CoalesceResult {
        for slot in queue.iter_mut().rev() {
            match slot {
                InputEnvelope::PointerEnter(existing)
                    if existing.tile_id == incoming.tile_id
                        && existing.node_id == incoming.node_id =>
                {
                    // Already have an enter — update to latest (same net state).
                    *existing = incoming;
                    return CoalesceResult::Merged;
                }
                InputEnvelope::PointerLeave(existing)
                    if existing.tile_id == incoming.tile_id
                        && existing.node_id == incoming.node_id =>
                {
                    // Have a leave, but net state is enter — replace.
                    *slot = InputEnvelope::PointerEnter(incoming);
                    return CoalesceResult::Merged;
                }
                _ => {}
            }
        }
        CoalesceResult::NotMerged
    }

    /// Coalesce an incoming `PointerLeave` against existing enter/leave events
    /// for the same node. Mirror of `coalesce_enter`.
    pub fn coalesce_leave(
        queue: &mut [InputEnvelope],
        incoming: PointerLeaveData,
    ) -> CoalesceResult {
        for slot in queue.iter_mut().rev() {
            match slot {
                InputEnvelope::PointerLeave(existing)
                    if existing.tile_id == incoming.tile_id
                        && existing.node_id == incoming.node_id =>
                {
                    *existing = incoming;
                    return CoalesceResult::Merged;
                }
                InputEnvelope::PointerEnter(existing)
                    if existing.tile_id == incoming.tile_id
                        && existing.node_id == incoming.node_id =>
                {
                    // Net state is leave — replace the enter.
                    *slot = InputEnvelope::PointerLeave(incoming);
                    return CoalesceResult::Merged;
                }
                _ => {}
            }
        }
        CoalesceResult::NotMerged
    }
}

/// A per-agent coalescing accumulator used during frame assembly.
///
/// Tracks the latest coalesced state for each ephemeral event category without
/// holding a reference to the full queue. Used by `EventBatchAssembler` when
/// building per-frame batches.
#[derive(Default)]
pub struct FrameCoalescer {
    /// Latest PointerMove per (tile_id, node_id).
    moves: HashMap<(SceneId, SceneId), PointerMoveData>,
    /// Latest ScrollOffset per tile_id.
    scrolls: HashMap<SceneId, ScrollOffsetChangedData>,
    /// Net hover state per (tile_id, node_id): true = entered, false = left.
    /// Only stored if the net state differs from "no event" (i.e., at least one
    /// enter or leave was received this frame).
    hover_net: HashMap<(SceneId, SceneId), HoverState>,
    /// Transactional events are always kept in order.
    transactional: Vec<InputEnvelope>,
}

#[derive(Debug, Clone)]
enum HoverState {
    Entered(PointerEnterData),
    Left(PointerLeaveData),
}

impl FrameCoalescer {
    /// Feed one event into the accumulator.
    ///
    /// Transactional events are buffered in order.
    /// Ephemeral events are coalesced per the spec rules.
    pub fn push(&mut self, envelope: InputEnvelope) {
        match envelope {
            InputEnvelope::PointerMove(d) => {
                let key = (d.tile_id, d.node_id);
                self.moves.insert(key, d);
            }
            InputEnvelope::ScrollOffsetChanged(d) => {
                self.scrolls.insert(d.tile_id, d);
            }
            InputEnvelope::PointerEnter(d) => {
                let key = (d.tile_id, d.node_id);
                self.hover_net.insert(key, HoverState::Entered(d));
            }
            InputEnvelope::PointerLeave(d) => {
                let key = (d.tile_id, d.node_id);
                self.hover_net.insert(key, HoverState::Left(d));
            }
            other if other.is_transactional() => {
                self.transactional.push(other);
            }
            other => {
                // Other ephemeral types — not yet coalesced per node.
                // Appended in push order; low-frequency so acceptable for v1.
                self.transactional.push(other);
            }
        }
    }

    /// Consume the accumulator and produce a flat list of events.
    ///
    /// The order within this list is NOT yet timestamp-sorted; callers (the
    /// `EventBatchAssembler`) sort by `timestamp_mono_us` before delivery.
    pub fn into_events(self) -> Vec<InputEnvelope> {
        let mut out: Vec<InputEnvelope> = Vec::new();

        // Transactional events (in push order).
        out.extend(self.transactional);

        // Coalesced PointerMove events (latest per node).
        for (_, d) in self.moves {
            out.push(InputEnvelope::PointerMove(d));
        }

        // Coalesced scroll events (latest per tile).
        for (_, d) in self.scrolls {
            out.push(InputEnvelope::ScrollOffsetChanged(d));
        }

        // Net hover state events.
        for (_, state) in self.hover_net {
            match state {
                HoverState::Entered(d) => out.push(InputEnvelope::PointerEnter(d)),
                HoverState::Left(d) => out.push(InputEnvelope::PointerLeave(d)),
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::*;
    use tze_hud_scene::{MonoUs, SceneId};

    fn null_id() -> SceneId {
        SceneId::null()
    }

    fn make_move(node_id: SceneId, ts: u64, x: f32, y: f32) -> InputEnvelope {
        InputEnvelope::PointerMove(PointerMoveData {
            tile_id: null_id(),
            node_id,
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(ts),
            device_id: String::new(),
            local_x: x,
            local_y: y,
            display_x: x,
            display_y: y,
        })
    }

    fn make_scroll(tile_id: SceneId, ts: u64, ox: f32, oy: f32) -> InputEnvelope {
        InputEnvelope::ScrollOffsetChanged(ScrollOffsetChangedData {
            tile_id,
            timestamp_mono_us: MonoUs(ts),
            offset_x: ox,
            offset_y: oy,
        })
    }

    fn make_enter(tile_id: SceneId, node_id: SceneId, ts: u64) -> InputEnvelope {
        InputEnvelope::PointerEnter(PointerEnterData {
            tile_id,
            node_id,
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(ts),
            device_id: String::new(),
            local_x: 0.0,
            local_y: 0.0,
        })
    }

    fn make_leave(tile_id: SceneId, node_id: SceneId, ts: u64) -> InputEnvelope {
        InputEnvelope::PointerLeave(PointerLeaveData {
            tile_id,
            node_id,
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(ts),
            device_id: String::new(),
        })
    }

    // ── EventCoalescer (in-place queue mutation) ───────────────────────────

    #[test]
    fn test_coalesce_move_replaces_existing() {
        let node_id = SceneId::new();
        let mut queue = vec![make_move(node_id, 100, 1.0, 1.0)];

        let incoming = PointerMoveData {
            tile_id: null_id(),
            node_id,
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(200),
            device_id: String::new(),
            local_x: 50.0,
            local_y: 60.0,
            display_x: 50.0,
            display_y: 60.0,
        };
        let result = EventCoalescer::coalesce_move(&mut queue, incoming);
        assert_eq!(result, CoalesceResult::Merged);
        assert_eq!(queue.len(), 1);
        if let InputEnvelope::PointerMove(d) = &queue[0] {
            assert_eq!(d.timestamp_mono_us, MonoUs(200));
            assert!((d.local_x - 50.0).abs() < 0.001);
        } else {
            panic!("expected PointerMove");
        }
    }

    #[test]
    fn test_coalesce_move_different_node_not_merged() {
        let node_a = SceneId::new();
        let node_b = SceneId::new();
        let mut queue = vec![make_move(node_a, 100, 1.0, 1.0)];

        let incoming = PointerMoveData {
            tile_id: null_id(),
            node_id: node_b,
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(200),
            device_id: String::new(),
            local_x: 5.0,
            local_y: 5.0,
            display_x: 5.0,
            display_y: 5.0,
        };
        let result = EventCoalescer::coalesce_move(&mut queue, incoming);
        assert_eq!(result, CoalesceResult::NotMerged);
        assert_eq!(queue.len(), 1); // unchanged
    }

    #[test]
    fn test_coalesce_scroll_latest_wins() {
        let tile = SceneId::new();
        let mut queue = vec![make_scroll(tile, 100, 0.0, 100.0)];

        let incoming = ScrollOffsetChangedData {
            tile_id: tile,
            timestamp_mono_us: MonoUs(200),
            offset_x: 0.0,
            offset_y: 250.0,
        };
        let result = EventCoalescer::coalesce_scroll(&mut queue, incoming);
        assert_eq!(result, CoalesceResult::Merged);
        if let InputEnvelope::ScrollOffsetChanged(d) = &queue[0] {
            assert!((d.offset_y - 250.0).abs() < 0.001);
        } else {
            panic!("expected ScrollOffsetChanged");
        }
    }

    #[test]
    fn test_coalesce_hover_leave_cancels_enter() {
        let tile = SceneId::new();
        let node = SceneId::new();
        let mut queue = vec![make_enter(tile, node, 100)];

        let leave = PointerLeaveData {
            tile_id: tile,
            node_id: node,
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(200),
            device_id: String::new(),
        };
        let result = EventCoalescer::coalesce_leave(&mut queue, leave);
        assert_eq!(result, CoalesceResult::Merged);
        assert_eq!(queue.len(), 1);
        assert!(matches!(queue[0], InputEnvelope::PointerLeave(_)));
    }

    #[test]
    fn test_coalesce_hover_enter_cancels_leave() {
        let tile = SceneId::new();
        let node = SceneId::new();
        let mut queue = vec![make_leave(tile, node, 100)];

        let enter = PointerEnterData {
            tile_id: tile,
            node_id: node,
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(200),
            device_id: String::new(),
            local_x: 0.0,
            local_y: 0.0,
        };
        let result = EventCoalescer::coalesce_enter(&mut queue, enter);
        assert_eq!(result, CoalesceResult::Merged);
        assert!(matches!(queue[0], InputEnvelope::PointerEnter(_)));
    }

    // ── FrameCoalescer ─────────────────────────────────────────────────────

    #[test]
    fn test_frame_coalescer_10_moves_one_survives() {
        let node = SceneId::new();
        let mut coalescer = FrameCoalescer::default();
        for i in 0..10u64 {
            coalescer.push(make_move(node, i * 1000, i as f32, i as f32));
        }
        let events = coalescer.into_events();
        let moves: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, InputEnvelope::PointerMove(_)))
            .collect();
        assert_eq!(
            moves.len(),
            1,
            "only one PointerMove should survive coalescing"
        );
        if let InputEnvelope::PointerMove(d) = &moves[0] {
            // Latest move was i=9
            assert!((d.local_x - 9.0).abs() < 0.001);
        }
    }

    #[test]
    fn test_frame_coalescer_scroll_latest_per_tile() {
        let tile = SceneId::new();
        let mut coalescer = FrameCoalescer::default();
        for i in 1..=5u64 {
            coalescer.push(make_scroll(tile, i * 1000, 0.0, i as f32 * 100.0));
        }
        let events = coalescer.into_events();
        let scrolls: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, InputEnvelope::ScrollOffsetChanged(_)))
            .collect();
        assert_eq!(scrolls.len(), 1);
        if let InputEnvelope::ScrollOffsetChanged(d) = &scrolls[0] {
            assert!((d.offset_y - 500.0).abs() < 0.001);
        }
    }

    #[test]
    fn test_frame_coalescer_hover_net_state() {
        let tile = SceneId::new();
        let node = SceneId::new();
        let mut coalescer = FrameCoalescer::default();
        // Enter then leave → net = leave
        coalescer.push(make_enter(tile, node, 100));
        coalescer.push(make_leave(tile, node, 200));
        let events = coalescer.into_events();
        let leaves: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, InputEnvelope::PointerLeave(_)))
            .collect();
        let enters: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, InputEnvelope::PointerEnter(_)))
            .collect();
        assert_eq!(leaves.len(), 1, "net state should be leave");
        assert_eq!(enters.len(), 0);
    }

    #[test]
    fn test_frame_coalescer_transactional_events_preserved() {
        let node = SceneId::new();
        let mut coalescer = FrameCoalescer::default();

        // Transactional down event
        coalescer.push(InputEnvelope::PointerDown(PointerDownData {
            tile_id: null_id(),
            node_id: node,
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(100),
            device_id: String::new(),
            local_x: 0.0,
            local_y: 0.0,
            display_x: 0.0,
            display_y: 0.0,
            button: 0,
        }));
        coalescer.push(InputEnvelope::PointerDown(PointerDownData {
            tile_id: null_id(),
            node_id: node,
            interaction_id: String::new(),
            timestamp_mono_us: MonoUs(200),
            device_id: String::new(),
            local_x: 0.0,
            local_y: 0.0,
            display_x: 0.0,
            display_y: 0.0,
            button: 0,
        }));

        let events = coalescer.into_events();
        let downs: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, InputEnvelope::PointerDown(_)))
            .collect();
        assert_eq!(
            downs.len(),
            2,
            "both transactional down events must survive"
        );
    }
}
