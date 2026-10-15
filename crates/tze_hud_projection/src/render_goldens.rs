//! Fixed visual fixtures captured before the scene renderer migration.
use crate::resident_grpc::{
    PortalVisualTokens, ResidentGrpcPortalAdapter, ResidentGrpcPortalConfig,
};
use crate::*;
use tze_hud_protocol::proto::{self, mutation_proto::Mutation};

fn view() -> PortalView {
    PortalView {
        portal_id: "golden".into(),
        presentation: ProjectedPortalPresentation::Expanded,
        redacted: false,
        connection_degraded: false,
        has_ever_connected: true,
        interaction_enabled: true,
        attention: ProjectedPortalAttention::Ambient,
        display_name: Some("Session".into()),
        lifecycle_state: Some(ProjectionLifecycleState::Active),
        status_text: None,
        visible_transcript: vec![],
        input_history: vec![],
        unread_output_count: Some(0),
        visible_unread_output_count: Some(0),
        pending_input_count: Some(0),
        latest_viewer_delivery_state: None,
        last_input_feedback: None,
        geometry_batch: None,
        resized_bounds: None,
    }
}
fn unit(text: &str, at: u64) -> TranscriptUnit {
    TranscriptUnit {
        sequence: 1,
        output_text: text.into(),
        output_kind: OutputKind::Assistant,
        expects_reply: false,
        appended_at_wall_us: at,
    }
}
fn snapshot(node: &proto::NodeProto, result: &mut String) {
    use std::fmt::Write;
    match node.data.as_ref().unwrap() {
        proto::node_proto::Data::TextMarkdown(text) => {
            writeln!(
                result,
                "text {:?} bounds {:?} font {:?} colour {:?} background {:?} runs {:?}",
                text.content,
                text.bounds,
                text.font_size_px,
                text.color,
                text.background,
                text.color_runs
            )
            .unwrap();
        }
        proto::node_proto::Data::SolidColor(solid) => {
            writeln!(result, "solid {:?}", solid).unwrap();
        }
        other => panic!("unexpected visual {other:?}"),
    }
    for child in &node.children {
        snapshot(child, result);
    }
}
#[test]
fn render_visual_goldens() {
    let now = 90_000_000;
    for name in [
        "empty",
        "connecting",
        "streaming",
        "quiesced",
        "unread",
        "expects_reply",
        "degraded",
        "replies",
        "truncated",
        "narrow",
        "rollover",
        "composer",
    ] {
        let mut state = view();
        let mut tokens = PortalVisualTokens::default();
        if !matches!(name, "empty" | "connecting") {
            state.visible_transcript.push(unit("Hello **world**", now));
        }
        match name {
            "connecting" => state.has_ever_connected = false,
            "quiesced" => state.visible_transcript[0].appended_at_wall_us = 1,
            "unread" => {
                state.visible_transcript.insert(0, unit("Earlier", 1));
                state.unread_output_count = Some(1);
                state.visible_unread_output_count = Some(1);
            }
            "expects_reply" => state.visible_transcript[0].expects_reply = true,
            "degraded" => {
                state.connection_degraded = true;
                state.interaction_enabled = false;
                state.lifecycle_state = Some(ProjectionLifecycleState::Degraded);
            }
            "replies" => {
                let mut reply = unit("Please continue", now);
                reply.output_kind = OutputKind::Viewer;
                state.input_history.push(reply);
                state.latest_viewer_delivery_state = Some(InputDeliveryState::Delivered);
            }
            "truncated" => {
                state.visible_transcript = (0..5).map(|_| unit(&"é".repeat(2048), now)).collect();
                state
                    .visible_transcript
                    .push(unit("Newest retained tail", now));
            }
            "narrow" => {
                state.resized_bounds = Some(AdapterPortalRect::from_f32(20.0, 40.0, 180.0, 140.0))
            }
            "rollover" => {
                tokens.timestamp_granularity = tze_hud_config::TimestampGranularity::PerTurn;
                state.visible_transcript = vec![
                    unit("Before midnight", 86_399_000_000),
                    unit("After midnight", 86_400_000_000),
                ];
            }
            _ => {}
        }
        let mut adapter = ResidentGrpcPortalAdapter::with_tokens(
            ResidentGrpcPortalConfig::new(vec![0; 16]),
            tokens,
        );
        adapter.record_created_tile(vec![1; 16]);
        if name == "composer" {
            adapter.apply_draft_notification(&AdapterDraftNotification {
                text: "Unsent local draft".into(),
                cursor: 6,
                selection_anchor: 6,
                at_capacity: false,
                sequence: 1,
            });
        }
        let batch = adapter.render_batch_with_surface(&state, now).unwrap();
        let mut actual = String::new();
        for mutation in batch.mutations {
            match mutation.mutation.unwrap() {
                Mutation::PublishToTile(publish) => {
                    snapshot(publish.node.as_ref().unwrap(), &mut actual)
                }
                Mutation::SetTileLifecycleAccent(accent) => {
                    actual.push_str(&format!(
                        "accent {:?} {:?}\n",
                        accent.color, accent.width_px
                    ));
                }
                Mutation::SetTileUnreadCount(unread) => {
                    actual.push_str(&format!("unread {}\n", unread.count))
                }
                _ => {}
            }
        }
        // Capture once against the old renderer; replaced by checked-in literals.
        println!("GOLDEN_START {name}\n{actual}GOLDEN_END");
    }
}
