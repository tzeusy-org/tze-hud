// ─── Traffic Class ───────────────────────────────────────────────────────────

use crate::proto::session::MutationBatch;
use crate::proto::session::server_message::Payload as ServerPayload;

/// Traffic class for outbound server messages.
///
/// Each class has different delivery guarantees:
/// - Transactional: at-least-once, ordered, never dropped.
/// - StateStream: at-least-once with coalescing; intermediate states may be skipped.
/// - Ephemeral: at-most-once, latest-wins, dropped under backpressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficClass {
    /// Reliable, ordered, never dropped. RequestResult, Reclaimed, SessionEstablished, etc.
    Transactional,
    /// Coalesced under pressure; intermediate states may be skipped. SceneSnapshot, EventBatch.
    StateStream,
    /// Droppable under backpressure; latest value wins. Heartbeat echo, ephemeral ZonePublish.
    Ephemeral,
}

/// Classify an outbound `ServerMessage` payload into its traffic class.
///
/// - Session lifecycle responses, RequestResult, Reclaimed,
///   SubscriptionChangeResult, Reclaimed,
///   SessionSuspended, SessionResumed, and input-control responses are Transactional.
/// - SceneSnapshot and EventBatch are StateStream.
/// - Heartbeat echoes are Ephemeral.
pub fn classify_server_payload(payload: &ServerPayload) -> TrafficClass {
    match payload {
        // Session lifecycle — always transactional
        ServerPayload::SessionEstablished(_)
        | ServerPayload::SessionError(_)
        | ServerPayload::SessionResumeResult(_)
        | ServerPayload::SessionSuspended(_)
        | ServerPayload::SessionResumed(_) => TrafficClass::Transactional,

        // Mutation / lease responses — transactional
        ServerPayload::RequestResult(_)
        | ServerPayload::Reclaimed(_)
        | ServerPayload::SubscriptionChangeResult(_)
        | ServerPayload::InputFocusResponse(_)
        | ServerPayload::InputCaptureResponse(_) => TrafficClass::Transactional,

        // Widget and resource-upload responses — transactional.
        ServerPayload::WidgetAssetRegisterResult(_)
        | ServerPayload::ResourceUploadAccepted(_)
        | ServerPayload::ResourceStored(_)
        | ServerPayload::ResourceErrorResponse(_) => TrafficClass::Transactional,

        // Degradation notice — transactional (never dropped)
        ServerPayload::DegradationNotice(_) => TrafficClass::Transactional,

        // Scene state / events — state-stream
        // FramePresented rides the telemetry class: coalesced/droppable under
        // backpressure (a present-latency probe samples it; hud-91uu6).
        ServerPayload::SceneSnapshot(_)
        | ServerPayload::EventBatch(_)
        | ServerPayload::FramePresented(_) => TrafficClass::StateStream,

        // Heartbeat echo — ephemeral (droppable, latest-wins)
        ServerPayload::Heartbeat(_) => TrafficClass::Ephemeral,

        // Element discovery response — transactional
        ServerPayload::ListElementsResponse(_) => TrafficClass::Transactional,

        // Element repositioned event — transactional (drag completion / reset-to-default)
        ServerPayload::ElementRepositioned(_) => TrafficClass::Transactional,
    }
}

// ─── Inbound mutation traffic class ──────────────────────────────────────────

/// Traffic class for an **inbound** `MutationBatch`.
///
/// Classify an inbound `MutationBatch` by examining its contained mutations.
///
/// Any structural/identity-changing mutation makes the batch Transactional;
/// otherwise content mutations are StateStream; empty batch is Ephemeral.
/// Uses the same `TrafficClass` enum as outbound classification.
pub(super) fn classify_inbound_batch(batch: &MutationBatch) -> TrafficClass {
    for m in &batch.mutations {
        if let Some(ref mutation) = m.mutation {
            use crate::proto::mutation_proto::Mutation;
            match mutation {
                Mutation::CreateTile(_) => return TrafficClass::Transactional,
                // AddNode is structural — marks the batch as Transactional.
                Mutation::AddNode(_) => return TrafficClass::Transactional,
                // SetTileRoot, UpdateTileOpacity, UpdateTileInputMode are StateStream.
                Mutation::SetTileRoot(_) => {}
                Mutation::UpdateTileOpacity(_) => {}
                Mutation::UpdateTileInputMode(_) => {}
                Mutation::PublishToTile(_) => {}
                // UpdateNodeContent is a content update — StateStream
                Mutation::UpdateNodeContent(_) => {}
                // Scroll mutations: config register is Transactional (structural),
                // offset updates are StateStream (rate-limited local feedback).
                Mutation::RegisterTileScroll(_) => {
                    return TrafficClass::Transactional;
                }
                Mutation::SetScrollOffset(_) => {}
                // Lifecycle accent is a content/state update — StateStream. It
                // reflects the portal's lifecycle and coalesces under pressure;
                // it must NOT mark the batch Transactional (hud-m48i0 / hud-mzk74),
                // which is what flipped lifecycle-visible portals off the
                // coalescible path when the accent was a per-republish AddNode.
                Mutation::SetTileLifecycleAccent(_) => {}
                // Ambient unread-output count for the jump-to-latest badge — a
                // coalescible content/state update, exactly like the lifecycle
                // accent above. It must NOT mark the batch Transactional so a
                // steady-state portal stays on the coalescible path (hud-hwk2m).
                Mutation::SetTileUnreadCount(_) => {}
                // Composer interaction hit region — a coalescible content/state
                // update, exactly like the lifecycle accent and unread count above.
                // The runtime derives the hit-region scene node from this overlay
                // state and re-attaches it after each transcript republish, so it
                // must NOT mark the batch Transactional: that is precisely what a
                // per-republish composer `AddNode` did, flipping an interaction-
                // enabled streaming portal off the coalescible path on the hottest
                // path (hud-mzk74 / hud-iofav).
                Mutation::SetTileComposerInteraction(_) => {}
                // Declaring/replacing the first-class portal surface is structural
                // (identity + parts) — Transactional.
                Mutation::SetPortalSurface(_) => return TrafficClass::Transactional,
                // Patching portal lifecycle/display state is a coalescible content
                // update — StateStream, exactly like the lifecycle accent above. It
                // must NOT mark the batch Transactional so steady-state portals stay
                // on the coalescible path (hud-mzk74).
                Mutation::UpdatePortalSurfaceState(_) => {}
            }
        }
    }
    // If we found any mutation at all, it's StateStream (content update)
    if batch.mutations.is_empty() {
        TrafficClass::Ephemeral
    } else {
        TrafficClass::StateStream
    }
}

#[cfg(test)]
mod inbound_tests {
    use super::*;
    use crate::proto::mutation_proto::Mutation;
    use crate::proto::{MutationProto, SetTileLifecycleAccentMutation, SetTileUnreadCountMutation};

    fn batch(mutations: Vec<Mutation>) -> MutationBatch {
        MutationBatch {
            batch_id: vec![0u8; 16],
            lease_id: vec![0u8; 16],
            mutations: mutations
                .into_iter()
                .map(|m| MutationProto { mutation: Some(m) })
                .collect(),
            timing: None,
        }
    }

    /// hud-m48i0 acceptance #2: a lifecycle-accent update riding an otherwise
    /// StateStream portal republish (PublishToTile + UpdateTileInputMode) must
    /// stay StateStream — the accent mutation must NOT mark the batch
    /// Transactional. This is the regression guard for hud-mzk74: the rejected
    /// per-republish `AddNode` accent flipped non-interactive lifecycle-visible
    /// portals off the coalescible path.
    #[test]
    fn lifecycle_accent_update_stays_state_stream() {
        let b = batch(vec![
            Mutation::PublishToTile(Default::default()),
            Mutation::UpdateTileInputMode(Default::default()),
            Mutation::SetTileLifecycleAccent(SetTileLifecycleAccentMutation {
                tile_id: vec![0u8; 16],
                color: None,
                width_px: 4.0,
            }),
        ]);
        assert_eq!(
            classify_inbound_batch(&b),
            TrafficClass::StateStream,
            "non-interactive lifecycle-visible portal republish must remain coalescible StateStream"
        );
    }

    /// hud-ga4md: a `SetTileRoot`/`PublishToTile` carrying an inline multi-node
    /// subtree (NodeProto.children) must stay StateStream. The whole point of
    /// inline children is that a multi-part portal body arrives as ONE
    /// coalescible mutation rather than a per-part `AddNode` fan-out — which
    /// classify_inbound_batch marks Transactional (line 128) and would knock the
    /// portal off the coalescible republish path (hud-mzk74). The children ride
    /// INSIDE the node, so they are invisible to classification: the guard here
    /// is that no new Transactional-forcing variant sneaks in.
    #[test]
    fn set_tile_root_with_inline_children_stays_state_stream() {
        use crate::proto::{NodeProto, SetTileRootMutation};
        let child = NodeProto {
            layout: 0,
            id: vec![],
            data: None,
            children: vec![],
        };
        let root = NodeProto {
            layout: 0,
            id: vec![],
            data: None,
            children: vec![child.clone(), child],
        };
        let b = batch(vec![
            Mutation::PublishToTile(Default::default()),
            Mutation::SetTileRoot(SetTileRootMutation {
                tile_id: vec![0u8; 16],
                node: Some(root),
            }),
        ]);
        assert_eq!(
            classify_inbound_batch(&b),
            TrafficClass::StateStream,
            "inline-subtree SetTileRoot must remain coalescible StateStream, never Transactional"
        );
    }

    /// A lifecycle-accent mutation on its own is a pure content update →
    /// StateStream (coalescible), never Transactional.
    #[test]
    fn lifecycle_accent_alone_is_state_stream() {
        let b = batch(vec![Mutation::SetTileLifecycleAccent(
            SetTileLifecycleAccentMutation {
                tile_id: vec![0u8; 16],
                color: None,
                width_px: 0.0,
            },
        )]);
        assert_eq!(classify_inbound_batch(&b), TrafficClass::StateStream);
    }

    /// hud-hwk2m: the jump-to-latest unread-count badge update, riding an
    /// otherwise-StateStream portal republish, must stay StateStream — exactly
    /// like the lifecycle accent. If it flipped the batch Transactional it would
    /// knock a bridged portal off the coalescible path under load.
    #[test]
    fn unread_count_update_stays_state_stream() {
        let b = batch(vec![
            Mutation::PublishToTile(Default::default()),
            Mutation::UpdateTileInputMode(Default::default()),
            Mutation::SetTileUnreadCount(SetTileUnreadCountMutation {
                tile_id: vec![0u8; 16],
                count: 4,
            }),
        ]);
        assert_eq!(
            classify_inbound_batch(&b),
            TrafficClass::StateStream,
            "a portal republish carrying the unread badge count must remain coalescible StateStream"
        );
    }

    /// hud-iofav: the interaction-path counterpart to the hud-mzk74 guard above.
    /// An interaction-enabled portal's FULL streaming republish — content
    /// (`PublishToTile`) + input mode + accent + the composer interaction hit region
    /// (`SetTileComposerInteraction`) + unread count — must stay StateStream. The
    /// composer rides coalescible overlay state, never a per-republish `AddNode`, so
    /// the hottest path (a streaming transcript with an interactive composer) stays
    /// on the latest-wins coalescible path under freeze/backpressure.
    #[test]
    fn interaction_enabled_streaming_republish_stays_state_stream() {
        use crate::proto::{HitRegionNodeProto, SetTileComposerInteractionMutation};
        let b = batch(vec![
            Mutation::PublishToTile(Default::default()),
            Mutation::UpdateTileInputMode(Default::default()),
            Mutation::SetTileLifecycleAccent(SetTileLifecycleAccentMutation {
                tile_id: vec![0u8; 16],
                color: None,
                width_px: 4.0,
            }),
            Mutation::SetTileComposerInteraction(SetTileComposerInteractionMutation {
                tile_id: vec![0u8; 16],
                composer: Some(HitRegionNodeProto {
                    accepts_composer_input: true,
                    accepts_pointer: true,
                    accepts_focus: true,
                    ..Default::default()
                }),
            }),
            Mutation::SetTileUnreadCount(SetTileUnreadCountMutation {
                tile_id: vec![0u8; 16],
                count: 2,
            }),
        ]);
        assert_eq!(
            classify_inbound_batch(&b),
            TrafficClass::StateStream,
            "an interaction-enabled streaming portal republish must remain coalescible \
             StateStream (composer rides overlay state, not a per-republish AddNode — hud-iofav)"
        );
    }

    /// A bare composer-interaction mutation is a pure content/state update →
    /// StateStream (coalescible), never Transactional. The clear form (absent
    /// composer) classifies identically.
    #[test]
    fn composer_interaction_alone_is_state_stream() {
        use crate::proto::SetTileComposerInteractionMutation;
        let b = batch(vec![Mutation::SetTileComposerInteraction(
            SetTileComposerInteractionMutation {
                tile_id: vec![0u8; 16],
                composer: None,
            },
        )]);
        assert_eq!(classify_inbound_batch(&b), TrafficClass::StateStream);
    }

    /// A bare unread-count mutation is a pure content update → StateStream.
    #[test]
    fn unread_count_alone_is_state_stream() {
        let b = batch(vec![Mutation::SetTileUnreadCount(
            SetTileUnreadCountMutation {
                tile_id: vec![0u8; 16],
                count: 0,
            },
        )]);
        assert_eq!(classify_inbound_batch(&b), TrafficClass::StateStream);
    }

    /// Sanity: a genuine structural `AddNode` still dominates the batch as
    /// Transactional even when a lifecycle accent rides alongside — the accent
    /// classification does not weaken structural-mutation guarantees.
    #[test]
    fn add_node_still_dominates_as_transactional() {
        let b = batch(vec![
            Mutation::SetTileLifecycleAccent(SetTileLifecycleAccentMutation {
                tile_id: vec![0u8; 16],
                color: None,
                width_px: 4.0,
            }),
            Mutation::AddNode(Default::default()),
        ]);
        assert_eq!(classify_inbound_batch(&b), TrafficClass::Transactional);
    }
}
