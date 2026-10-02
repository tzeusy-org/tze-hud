//! Abstract command input model per RFC 0004 §10.
//!
//! Implements:
//! - `CommandAction`       — seven abstract commands (NAVIGATE_NEXT, …, SCROLL_DOWN)
//! - `CommandSource`       — input device class (KEYBOARD, DPAD, VOICE, …)
//! - `RawCommandEvent`     — raw abstract command from the OS / input layer
//! - `CommandInputEvent`   — agent-facing command event (all required fields)
//! - `CommandDispatch`     — routing descriptor for the owning agent
//! - `CommandProcessor`    — maps raw commands to `CommandDispatch` + ACTIVATE local feedback
//!
//! # Transactional semantics (spec line 379)
//! `CommandInputEvent` is a **transactional** event. It MUST NEVER be coalesced
//! or dropped under backpressure. The event batching layer (bead rig-gnjc) is
//! responsible for honouring this; this module marks all command events with
//! `is_transactional: true` in `CommandDispatch`.
//!
//! # ACTIVATE local feedback (spec lines 389–391)
//! When `action == CommandAction::Activate`, the processor:
//! 1. Sets `pressed = true` on the focused `HitRegionNode` via `SceneGraph`
//!    (same as `PointerDownEvent` in Stage 2).
//! 2. Returns a `CommandDispatch` with `activate_pressed_state = true` so the
//!    compositor thread can apply the `SceneLocalPatch`.
//!    Latency budget: `input_to_local_ack` p99 < 4ms (same as pointer).
//!
//! # NAVIGATE_NEXT with no focus (spec lines 323–326)
//! When `action == CommandAction::NavigateNext` and focus is `FocusOwner::None`,
//! `CommandProcessor::process` returns `None` (no dispatch). The caller is
//! responsible for first driving `FocusManager::navigate_next` to advance focus
//! to the first focusable element, then calling `process` again with the updated
//! `FocusOwner` so routing can proceed for the newly focused element.
//!
//! # Spec refs
//! - Lines 378–380: Command Input Model
//! - Lines 383–385: D-pad maps to NAVIGATE_NEXT
//! - Lines 389–391: ACTIVATE Local Feedback
//! - Lines 411–413: Pointer-Free Navigation

use serde::{Deserialize, Serialize};
use std::time::Instant;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::{MonoUs, NodeData, SceneId};

use crate::focus_tree::FocusOwner;

// ─── Command action ────────────────────────────────────────────────────────────

/// Seven abstract command actions (spec line 378).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CommandAction {
    /// Move focus to the next focusable element in tab order.
    NavigateNext,
    /// Move focus to the previous focusable element in tab order.
    NavigatePrev,
    /// Activate the focused element (equivalent to a click / primary action).
    Activate,
    /// Cancel / dismiss the focused element or current operation.
    Cancel,
    /// Open the context menu for the focused element.
    Context,
    /// Scroll up in the focused scrollable region.
    ScrollUp,
    /// Scroll down in the focused scrollable region.
    ScrollDown,
}

// ─── Command source ────────────────────────────────────────────────────────────

/// The physical or virtual input device class that generated this command
/// (spec line 379).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CommandSource {
    /// Generated from a physical keyboard (Tab, Enter, Escape, Arrow keys, …).
    Keyboard,
    /// D-pad on a remote, controller, or wearable device.
    Dpad,
    /// Voice command from an ASR pipeline.
    Voice,
    /// Remote clicker / presentation remote.
    RemoteClicker,
    /// Rotary dial (smart watches, HID dials).
    RotaryDial,
    /// Programmatically injected by the runtime or a test harness.
    Programmatic,
}

// ─── Raw command event ─────────────────────────────────────────────────────────

/// Raw command event from the OS / input preprocessing layer.
///
/// This is the input to `CommandProcessor::process`. The processor maps it to a
/// `CommandDispatch` using the current focus state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawCommandEvent {
    pub action: CommandAction,
    pub source: CommandSource,
    /// Hardware device identifier (e.g. HID device path / index).
    pub device_id: String,
    /// Monotonic timestamp in microseconds.
    pub timestamp_mono_us: MonoUs,
}

// ─── Agent-facing event ────────────────────────────────────────────────────────

/// Agent-facing `CommandInputEvent` as defined in spec line 379.
///
/// Carries all required fields: tile_id, node_id, interaction_id,
/// timestamp_mono_us, device_id, action, source.
#[derive(Clone, Debug)]
pub struct CommandInputEvent {
    pub tile_id: SceneId,
    /// None for tile-level focus (no node).
    pub node_id: Option<SceneId>,
    /// Interaction ID of the focused `HitRegionNode`, or empty string.
    pub interaction_id: String,
    pub timestamp_mono_us: MonoUs,
    pub device_id: String,
    pub action: CommandAction,
    pub source: CommandSource,
}

// ─── Routing descriptor ────────────────────────────────────────────────────────

/// Descriptor produced by `CommandProcessor::process` for routing to the
/// owning agent.
#[derive(Clone, Debug)]
pub struct CommandDispatch {
    /// Agent namespace that owns the focused tile.
    pub namespace: String,
    /// The event payload to deliver.
    pub event: CommandInputEvent,
    /// MUST be true for all command dispatches. The event batching layer (bead
    /// rig-gnjc) uses this flag to never coalesce or drop the event.
    pub is_transactional: bool,
    /// True iff the focused `HitRegionNode` should have `pressed = true` applied
    /// as a `SceneLocalPatch` in Stage 2 (ACTIVATE local feedback).
    pub activate_pressed_state: bool,
    /// Time from event ingestion to dispatch construction (microseconds).
    pub local_ack_us: u64,
}

// ─── Command processor ────────────────────────────────────────────────────────

/// Maps raw command events to `CommandDispatch` descriptors using current focus
/// state.
///
/// The processor is stateless with respect to focus; the runtime kernel passes
/// the current `FocusOwner` for the active tab on each call.
///
/// # ACTIVATE local feedback
/// For `CommandAction::Activate`, the processor sets `pressed = true` on the
/// focused `HitRegionNode` in the `SceneGraph` (Stage 2 local feedback, same
/// path as `PointerDownEvent`). The caller is responsible for rolling back the
/// pressed state if the agent rejects the activation.
pub struct CommandProcessor;

impl CommandProcessor {
    pub fn new() -> Self {
        Self
    }

    /// Process a raw command event and produce a `CommandDispatch` if a tile or
    /// node has focus.
    ///
    /// Returns `None` when:
    /// - Focus is `FocusOwner::None` **and** the action is not `NavigateNext`/
    ///   `NavigatePrev` (those advance focus before routing, so the caller must
    ///   call `FocusManager::navigate_next/prev` first then re-dispatch).
    /// - Focus is on a chrome element.
    /// - The tile namespace cannot be resolved.
    ///
    /// # NAVIGATE_NEXT with no focus
    /// When focus is `None` and `action == NavigateNext`, the processor returns
    /// `None`. The caller must first drive `FocusManager::navigate_next`, obtain
    /// the newly focused element, and then call `process` again with the updated
    /// focus owner. This two-step model keeps the processor decoupled from the
    /// focus manager.
    pub fn process(
        &self,
        event: &RawCommandEvent,
        focus: &FocusOwner,
        scene: &mut SceneGraph,
        namespace_for_tile: impl Fn(SceneId) -> Option<String>,
    ) -> Option<CommandDispatch> {
        let start = Instant::now();

        let (tile_id, node_id) = match focus {
            FocusOwner::None | FocusOwner::ChromeElement(_) => return None,
            FocusOwner::Tile(tid) => (*tid, None),
            FocusOwner::Node { tile_id, node_id } => (*tile_id, Some(*node_id)),
        };

        let namespace = namespace_for_tile(tile_id)?;

        // Collect interaction_id before mutable borrow.
        let interaction_id = node_id
            .and_then(|nid| scene.nodes.get(&nid))
            .and_then(|n| {
                if let NodeData::HitRegion(hr) = &n.data {
                    Some(hr.interaction_id.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        // ── ACTIVATE local feedback (spec lines 389–391) ──────────────────
        let activate_pressed_state = event.action == CommandAction::Activate;
        if activate_pressed_state {
            if let Some(nid) = node_id {
                if let Some(state) = scene.hit_region_states.get_mut(&nid) {
                    state.pressed = true;
                }
            }
        }

        let local_ack_us = start.elapsed().as_micros() as u64;

        Some(CommandDispatch {
            namespace,
            event: CommandInputEvent {
                tile_id,
                node_id,
                interaction_id,
                timestamp_mono_us: event.timestamp_mono_us,
                device_id: event.device_id.clone(),
                action: event.action,
                source: event.source,
            },
            is_transactional: true,
            activate_pressed_state,
            local_ack_us,
        })
    }
}

impl Default for CommandProcessor {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::{HitRegionNode, Node, NodeData, Rect, SceneGraph, SceneId};

    // ── Test scene helpers ────────────────────────────────────────────────────

    fn setup_scene_with_focused_node() -> (SceneGraph, SceneId, SceneId, SceneId) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("test-agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "test-agent",
                lease_id,
                Rect::new(0.0, 0.0, 400.0, 300.0),
                1,
            )
            .unwrap();

        let node_id = SceneId::new();
        let node = Node {
            layout: Default::default(),
            id: node_id,
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(10.0, 10.0, 100.0, 50.0),
                interaction_id: "btn-submit".to_string(),
                accepts_focus: true,
                accepts_pointer: true,
                ..Default::default()
            }),
        };
        scene.set_tile_root(tile_id, node).unwrap();

        (scene, tab_id, tile_id, node_id)
    }

    fn raw_command(action: CommandAction, source: CommandSource) -> RawCommandEvent {
        RawCommandEvent {
            action,
            source,
            device_id: "dpad-0".to_string(),
            timestamp_mono_us: MonoUs(10_000),
        }
    }

    // ── Spec scenario (line 384): D-pad NAVIGATE_NEXT dispatches CommandInputEvent ──

    /// WHEN glasses temple D-pad down button pressed with focus on HitRegionNode
    /// THEN runtime dispatches CommandInputEvent(action=NAVIGATE_NEXT, source=DPAD).
    #[test]
    fn dpad_navigate_next_dispatches_command_event() {
        let (mut scene, _, tile_id, node_id) = setup_scene_with_focused_node();
        let focus = FocusOwner::Node { tile_id, node_id };
        let proc = CommandProcessor::new();

        let dispatch = proc
            .process(
                &raw_command(CommandAction::NavigateNext, CommandSource::Dpad),
                &focus,
                &mut scene,
                |_| Some("test-agent".to_string()),
            )
            .expect("expected CommandDispatch");

        assert_eq!(dispatch.event.action, CommandAction::NavigateNext);
        assert_eq!(dispatch.event.source, CommandSource::Dpad);
        assert_eq!(dispatch.event.tile_id, tile_id);
        assert_eq!(dispatch.event.node_id, Some(node_id));
        assert!(dispatch.is_transactional);
    }

    // ── Spec scenario (line 395): ACTIVATE produces pressed state ────────────

    /// WHEN CommandInputEvent(action=ACTIVATE) arrives for a focused HitRegionNode
    /// THEN runtime sets pressed=true on the node in the same frame.
    #[test]
    fn activate_sets_pressed_state() {
        let (mut scene, _, tile_id, node_id) = setup_scene_with_focused_node();
        let focus = FocusOwner::Node { tile_id, node_id };
        let proc = CommandProcessor::new();

        let dispatch = proc
            .process(
                &raw_command(CommandAction::Activate, CommandSource::Keyboard),
                &focus,
                &mut scene,
                |_| Some("test-agent".to_string()),
            )
            .expect("expected CommandDispatch for ACTIVATE");

        // Local feedback: pressed state should be set.
        assert!(
            scene.hit_region_states[&node_id].pressed,
            "ACTIVATE must set pressed=true on the focused node"
        );
        assert!(dispatch.activate_pressed_state);
        assert_eq!(dispatch.event.action, CommandAction::Activate);
    }

    /// ACTIVATE via DPAD also sets pressed state (source is irrelevant for local feedback).
    #[test]
    fn activate_via_dpad_also_sets_pressed_state() {
        let (mut scene, _, tile_id, node_id) = setup_scene_with_focused_node();
        let focus = FocusOwner::Node { tile_id, node_id };
        let proc = CommandProcessor::new();

        proc.process(
            &raw_command(CommandAction::Activate, CommandSource::Dpad),
            &focus,
            &mut scene,
            |_| Some("test-agent".to_string()),
        )
        .expect("dispatch");

        assert!(scene.hit_region_states[&node_id].pressed);
    }

    /// Non-ACTIVATE commands do NOT set pressed state.
    #[test]
    fn non_activate_commands_do_not_set_pressed() {
        let (mut scene, _, tile_id, node_id) = setup_scene_with_focused_node();
        let focus = FocusOwner::Node { tile_id, node_id };
        let proc = CommandProcessor::new();

        proc.process(
            &raw_command(CommandAction::Cancel, CommandSource::Keyboard),
            &focus,
            &mut scene,
            |_| Some("test-agent".to_string()),
        )
        .expect("dispatch");

        assert!(!scene.hit_region_states[&node_id].pressed);
    }

    // ── All seven actions ─────────────────────────────────────────────────────

    /// All seven abstract commands produce a CommandDispatch.
    #[test]
    fn all_seven_actions_produce_dispatch() {
        let actions = [
            CommandAction::NavigateNext,
            CommandAction::NavigatePrev,
            CommandAction::Activate,
            CommandAction::Cancel,
            CommandAction::Context,
            CommandAction::ScrollUp,
            CommandAction::ScrollDown,
        ];

        for action in actions {
            let (mut scene, _, tile_id, node_id) = setup_scene_with_focused_node();
            let focus = FocusOwner::Node { tile_id, node_id };
            let proc = CommandProcessor::new();

            let dispatch = proc.process(
                &raw_command(action, CommandSource::Programmatic),
                &focus,
                &mut scene,
                |_| Some("ns".to_string()),
            );

            assert!(
                dispatch.is_some(),
                "action {action:?} should produce a CommandDispatch"
            );
            assert_eq!(dispatch.unwrap().event.action, action);
        }
    }

    // ── interaction_id field ──────────────────────────────────────────────────

    /// interaction_id is populated from the focused HitRegionNode.
    #[test]
    fn interaction_id_populated_from_node() {
        let (mut scene, _, tile_id, node_id) = setup_scene_with_focused_node();
        let focus = FocusOwner::Node { tile_id, node_id };
        let proc = CommandProcessor::new();

        let dispatch = proc
            .process(
                &raw_command(CommandAction::Cancel, CommandSource::Voice),
                &focus,
                &mut scene,
                |_| Some("test-agent".to_string()),
            )
            .unwrap();

        assert_eq!(dispatch.event.interaction_id, "btn-submit");
    }

    /// Tile-level focus produces empty interaction_id.
    #[test]
    fn tile_focus_has_empty_interaction_id() {
        let (mut scene, _, tile_id, _) = setup_scene_with_focused_node();
        let focus = FocusOwner::Tile(tile_id);
        let proc = CommandProcessor::new();

        let dispatch = proc
            .process(
                &raw_command(CommandAction::ScrollUp, CommandSource::RotaryDial),
                &focus,
                &mut scene,
                |_| Some("test-agent".to_string()),
            )
            .unwrap();

        assert_eq!(dispatch.event.interaction_id, "");
        assert_eq!(dispatch.event.node_id, None);
    }

    // ── No dispatch conditions ────────────────────────────────────────────────

    /// No dispatch when focus is None.
    #[test]
    fn no_dispatch_when_focus_none() {
        let (mut scene, _, _, _) = setup_scene_with_focused_node();
        let focus = FocusOwner::None;
        let proc = CommandProcessor::new();

        let result = proc.process(
            &raw_command(CommandAction::Activate, CommandSource::Keyboard),
            &focus,
            &mut scene,
            |_| Some("ns".to_string()),
        );
        assert!(result.is_none());
    }

    /// No dispatch when focus is on a chrome element.
    #[test]
    fn no_dispatch_when_focus_chrome() {
        let (mut scene, _, _, _) = setup_scene_with_focused_node();
        let chrome_id = SceneId::new();
        let focus = FocusOwner::ChromeElement(chrome_id);
        let proc = CommandProcessor::new();

        let result = proc.process(
            &raw_command(CommandAction::Activate, CommandSource::Keyboard),
            &focus,
            &mut scene,
            |_| Some("ns".to_string()),
        );
        assert!(result.is_none());
    }

    // ── Transactional flag ────────────────────────────────────────────────────

    /// All command dispatches carry is_transactional = true.
    #[test]
    fn command_dispatch_is_always_transactional() {
        let (mut scene, _, tile_id, node_id) = setup_scene_with_focused_node();
        let focus = FocusOwner::Node { tile_id, node_id };
        let proc = CommandProcessor::new();

        let dispatch = proc
            .process(
                &raw_command(CommandAction::ScrollDown, CommandSource::Dpad),
                &focus,
                &mut scene,
                |_| Some("ns".to_string()),
            )
            .unwrap();

        assert!(
            dispatch.is_transactional,
            "CommandInputEvent must always be transactional (never coalesced or dropped)"
        );
    }

    // ── device_id and timestamp fields ────────────────────────────────────────

    /// device_id and timestamp_mono_us are preserved in the dispatch.
    #[test]
    fn device_id_and_timestamp_preserved() {
        let (mut scene, _, tile_id, node_id) = setup_scene_with_focused_node();
        let focus = FocusOwner::Node { tile_id, node_id };
        let proc = CommandProcessor::new();

        let event = RawCommandEvent {
            action: CommandAction::Cancel,
            source: CommandSource::RemoteClicker,
            device_id: "remote-42".to_string(),
            timestamp_mono_us: MonoUs(999_888),
        };

        let dispatch = proc
            .process(&event, &focus, &mut scene, |_| Some("ns".to_string()))
            .unwrap();

        assert_eq!(dispatch.event.device_id, "remote-42");
        assert_eq!(dispatch.event.timestamp_mono_us, MonoUs(999_888));
    }

    // ── Latency budget ────────────────────────────────────────────────────────

    /// ACTIVATE local_ack_us must be within the 4ms budget.
    #[test]
    fn activate_local_ack_within_4ms() {
        use tze_hud_scene::perf_budget::{budgets, test_budget};

        let (mut scene, _, tile_id, node_id) = setup_scene_with_focused_node();
        let focus = FocusOwner::Node { tile_id, node_id };
        let proc = CommandProcessor::new();

        let dispatch = proc
            .process(
                &raw_command(CommandAction::Activate, CommandSource::Keyboard),
                &focus,
                &mut scene,
                |_| Some("ns".to_string()),
            )
            .unwrap();

        let budget = test_budget(budgets::INPUT_ACK_BUDGET_US);
        assert!(
            dispatch.local_ack_us < budget,
            "ACTIVATE local_ack_us was {}us, calibrated budget is {}us",
            dispatch.local_ack_us,
            budget,
        );
    }

    // ── Pointer-free navigation (spec lines 411–413) ─────────────────────────

    /// All pointer interactions are achievable via command input.
    /// This test verifies click (ACTIVATE), context (CONTEXT), scroll (SCROLL_UP/DOWN),
    /// and cancel (CANCEL) all produce valid dispatches from a focused node.
    #[test]
    fn pointer_free_navigation_all_equivalents_dispatch() {
        let equivalents = [
            (CommandAction::Activate, "click"),
            (CommandAction::Context, "context menu"),
            (CommandAction::ScrollUp, "scroll up"),
            (CommandAction::ScrollDown, "scroll down"),
            (CommandAction::Cancel, "tab close"),
        ];

        for (action, label) in equivalents {
            let (mut scene, _, tile_id, node_id) = setup_scene_with_focused_node();
            let focus = FocusOwner::Node { tile_id, node_id };
            let proc = CommandProcessor::new();

            let result = proc.process(
                &raw_command(action, CommandSource::Dpad),
                &focus,
                &mut scene,
                |_| Some("ns".to_string()),
            );

            assert!(
                result.is_some(),
                "pointer-free equivalent '{label}' must produce a dispatch"
            );
        }
    }
}
