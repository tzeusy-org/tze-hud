//! Message round-trip serialization tests for types.proto, events.proto, session.proto.
//!
//! Every message type: construct → serialize (prost::Message::encode) →
//! deserialize (prost::Message::decode) → assert field equality.
//!
//! Edge cases: empty fields, max u64/u32/float values, all enum variants,
//! nested messages. Proto3 forward-compatibility: decoding tolerates unknown
//! fields (validated via raw bytes), but prost does not preserve them on re-encode.
//!
//! Test count target: ≥30 functions.

use prost::Message;
use tze_hud_protocol::proto::input_envelope::Event as InputEnvelopeEvent;
use tze_hud_protocol::proto::mutation_proto::Mutation;
use tze_hud_protocol::proto::node_proto::Data as NodeData;
use tze_hud_protocol::proto::session::auth_credential::Credential;
use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
use tze_hud_protocol::proto::session::{
    AuthCredential,
    // session.proto
    ClientMessage,
    ErrorCode,
    Heartbeat,
    LeaseRequest,
    LeaseResponse,
    LeaseResult,
    LocalSocketCredential,
    MutationBatch,
    PreSharedKeyCredential,
    ResolvedPortalTokens,
    RuntimeError,
    SceneSnapshot,
    ServerMessage,
    SessionClose,
    SessionError,
    SessionEstablished,
    SessionInit,
    SessionResume,
    SessionResumeResult,
    SubscriptionChange,
    SubscriptionEntry,
    TimingHints,
};
use tze_hud_protocol::proto::zone_content::Payload as ZonePayload;
use tze_hud_protocol::proto::{
    CaptureReleasedEvent,
    CaptureReleasedReason,
    ClearWidgetMutation,
    ClearZoneMutation,
    CommandAction,
    CommandInputEvent,
    CommandSource,
    ContentionPolicyProto,
    CreateTileMutation,
    EventBatch,
    FocusGainedEvent,
    FocusLostEvent,
    FocusLostReason,
    FocusSource,
    FontFamilyProto,
    GeometryPolicyProto,
    GestureEvent,
    HitRegionNodeProto,
    ImageFitModeProto,
    ImeCompositionEndEvent,
    ImeCompositionStartEvent,
    ImeCompositionUpdateEvent,
    InputEnvelope,
    KeyDownEvent,
    KeyUpEvent,
    MutationProto,
    NodeProto,
    NotificationActionProto,
    NotificationPayload,
    PointerDownEvent,
    PointerMoveEvent,
    PointerUpEvent,
    PublishToZoneMutation,
    // types.proto
    Rect,
    RelativeGeometryPolicy,
    RenderingPolicyProto,
    Rgba,
    ScrollOffsetChangedEvent,
    SetTileRootMutation,
    SolidColorNodeProto,
    StaticImageNodeProto,
    StatusBarPayload,
    TextAlignProto,
    TextMarkdownNodeProto,
    TextOverflowProto,
    ZoneContent,
    ZoneDefinitionProto,
};

/// Encode then decode a prost Message and return the decoded value.
fn round_trip<T: Message + Default>(msg: &T) -> T {
    let mut buf = Vec::new();
    msg.encode(&mut buf).expect("encode must succeed");
    T::decode(buf.as_slice()).expect("decode must succeed")
}

// ─── types.proto ─────────────────────────────────────────────────────────────

#[test]
fn roundtrip_rect() {
    let orig = Rect {
        x: 1.5,
        y: 2.5,
        width: 100.0,
        height: 200.0,
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.x, decoded.x);
    assert_eq!(orig.y, decoded.y);
    assert_eq!(orig.width, decoded.width);
    assert_eq!(orig.height, decoded.height);
}

#[test]
fn roundtrip_rect_max_values() {
    let orig = Rect {
        x: f32::MAX,
        y: f32::MIN,
        width: f32::INFINITY,
        height: f32::NEG_INFINITY,
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.x, decoded.x);
    assert_eq!(orig.y, decoded.y);
    assert!(decoded.width.is_infinite() && decoded.width > 0.0);
    assert!(decoded.height.is_infinite() && decoded.height < 0.0);
}

#[test]
fn roundtrip_rgba() {
    let orig = Rgba {
        r: 0.5,
        g: 0.25,
        b: 0.75,
        a: 1.0,
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.r, decoded.r);
    assert_eq!(orig.g, decoded.g);
    assert_eq!(orig.b, decoded.b);
    assert_eq!(orig.a, decoded.a);
}

#[test]
fn roundtrip_rgba_zero() {
    let orig = Rgba::default();
    let decoded = round_trip(&orig);
    assert_eq!(orig.r, decoded.r);
    assert_eq!(orig.a, decoded.a);
}

#[test]
fn roundtrip_node_proto_solid_color() {
    let orig = NodeProto {
        layout: 0,
        id: b"node-abc".to_vec(),
        data: Some(NodeData::SolidColor(SolidColorNodeProto {
            color: Some(Rgba {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            }),
            bounds: Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 50.0,
                height: 50.0,
            }),
            radius: 12.0,
        })),
        children: vec![],
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.id, decoded.id);
    match &decoded.data {
        Some(NodeData::SolidColor(sc)) => {
            assert_eq!(sc.color.as_ref().unwrap().r, 1.0);
            assert_eq!(sc.radius, 12.0);
        }
        _ => panic!("wrong data variant"),
    }
}

#[test]
fn roundtrip_node_proto_text_markdown() {
    let orig = NodeProto {
        layout: 0,
        id: b"node-text".to_vec(),
        data: Some(NodeData::TextMarkdown(TextMarkdownNodeProto {
            content: "**hello world**".to_string(),
            bounds: Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 100.0,
            }),
            font_size_px: 14.0,
            color: Some(Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            }),
            background: Some(Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 0.0,
            }),
            color_runs: vec![],
            overflow: 0, // Unspecified = proto3 default
        })),
        children: vec![],
    };
    let decoded = round_trip(&orig);
    match &decoded.data {
        Some(NodeData::TextMarkdown(tm)) => {
            assert_eq!(tm.content, "**hello world**");
            assert_eq!(tm.font_size_px, 14.0);
            // Backward-compat: no color_runs → empty vec on decode.
            assert!(
                tm.color_runs.is_empty(),
                "empty color_runs must round-trip as empty"
            );
        }
        _ => panic!("wrong data variant"),
    }
}

/// Proto roundtrip: TextMarkdownNodeProto with two color runs survives encode/decode.
///
/// Field number 6 (color_runs) is stable and covered by this test.
/// Acceptance criterion AC-4 [hud-r52v].
#[test]
fn roundtrip_node_proto_text_markdown_with_color_runs() {
    use tze_hud_protocol::proto::TextColorRunProto;

    let orig = NodeProto {
        layout: 0,
        id: b"node-colored".to_vec(),
        data: Some(NodeData::TextMarkdown(TextMarkdownNodeProto {
            content: "ERROR: disk full".to_string(),
            bounds: Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 300.0,
                height: 60.0,
            }),
            font_size_px: 14.0,
            color: Some(Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            }),
            background: None,
            color_runs: vec![
                TextColorRunProto {
                    start_byte: 0,
                    end_byte: 5, // "ERROR"
                    color: Some(Rgba {
                        r: 1.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    }),
                },
                TextColorRunProto {
                    start_byte: 7,
                    end_byte: 16, // "disk full"
                    color: Some(Rgba {
                        r: 1.0,
                        g: 1.0,
                        b: 0.0,
                        a: 1.0,
                    }),
                },
            ],
            overflow: 0, // Unspecified = proto3 default
        })),
        children: vec![],
    };

    let decoded = round_trip(&orig);
    match &decoded.data {
        Some(NodeData::TextMarkdown(tm)) => {
            assert_eq!(tm.content, "ERROR: disk full");
            assert_eq!(
                tm.color_runs.len(),
                2,
                "two color runs must survive roundtrip"
            );
            // First run: "ERROR" in red
            assert_eq!(tm.color_runs[0].start_byte, 0);
            assert_eq!(tm.color_runs[0].end_byte, 5);
            let r0 = tm.color_runs[0].color.as_ref().expect("color must be set");
            assert!((r0.r - 1.0).abs() < 1e-5, "R should be 1.0 (red)");
            assert!(r0.g.abs() < 1e-5, "G should be 0.0");
            // Second run: "disk full" in yellow
            assert_eq!(tm.color_runs[1].start_byte, 7);
            assert_eq!(tm.color_runs[1].end_byte, 16);
        }
        _ => panic!("wrong data variant"),
    }
}

/// Convert roundtrip: proto → scene → proto preserves color runs.
///
/// Acceptance criterion AC-3 (convert roundtrip) [hud-r52v].
#[test]
fn convert_roundtrip_color_runs() {
    use tze_hud_protocol::convert::{proto_color_runs_to_scene, scene_color_runs_to_proto};
    use tze_hud_protocol::proto::TextColorRunProto;

    let proto_runs = vec![
        TextColorRunProto {
            start_byte: 0,
            end_byte: 5,
            color: Some(tze_hud_protocol::proto::Rgba {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            }),
        },
        TextColorRunProto {
            start_byte: 7,
            end_byte: 16,
            color: Some(tze_hud_protocol::proto::Rgba {
                r: 0.0,
                g: 1.0,
                b: 0.0,
                a: 1.0,
            }),
        },
    ];

    // Proto → scene
    let scene_runs = proto_color_runs_to_scene(&proto_runs);
    assert_eq!(scene_runs.len(), 2);
    assert_eq!(scene_runs[0].start_byte, 0);
    assert_eq!(scene_runs[0].end_byte, 5);
    assert!((scene_runs[0].color.r - 1.0).abs() < 1e-5);

    // Scene → proto
    let back_to_proto = scene_color_runs_to_proto(&scene_runs);
    assert_eq!(back_to_proto.len(), 2);
    assert_eq!(back_to_proto[0].start_byte, 0);
    assert_eq!(back_to_proto[0].end_byte, 5);
    let c = back_to_proto[0].color.as_ref().unwrap();
    assert!((c.r - 1.0).abs() < 1e-5);

    // Empty round-trip
    let empty: Box<[tze_hud_scene::types::TextColorRun]> = Box::default();
    assert!(proto_color_runs_to_scene(&[]).is_empty());
    assert!(scene_color_runs_to_proto(&empty).is_empty());
}

#[test]
fn roundtrip_node_proto_hit_region() {
    let orig = NodeProto {
        layout: 0,
        id: b"hit-1".to_vec(),
        data: Some(NodeData::HitRegion(HitRegionNodeProto {
            bounds: Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 50.0,
                height: 50.0,
            }),
            interaction_id: "btn-primary".to_string(),
            accepts_focus: true,
            accepts_pointer: true,
            auto_capture: true,
            release_on_up: true,
            accepts_composer_input: false,
        })),
        children: vec![],
    };
    let decoded = round_trip(&orig);
    match &decoded.data {
        Some(NodeData::HitRegion(hr)) => {
            assert_eq!(hr.interaction_id, "btn-primary");
            assert!(hr.accepts_focus);
            assert!(hr.accepts_pointer);
            assert!(hr.auto_capture);
            assert!(hr.release_on_up);
        }
        _ => panic!("wrong data variant"),
    }
}

#[test]
fn roundtrip_node_proto_static_image_all_fit_modes() {
    for &fit in &[
        ImageFitModeProto::ImageFitModeUnspecified,
        ImageFitModeProto::ImageFitModeContain,
        ImageFitModeProto::ImageFitModeCover,
        ImageFitModeProto::ImageFitModeFill,
        ImageFitModeProto::ImageFitModeScaleDown,
    ] {
        let orig = NodeProto {
            layout: 0,
            id: b"img-node".to_vec(),
            data: Some(NodeData::StaticImage(StaticImageNodeProto {
                resource_id: vec![0xAB; 32],
                width: 1920,
                height: 1080,
                decoded_bytes: 1920 * 1080 * 4,
                fit_mode: fit as i32,
                bounds: Some(Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                }),
            })),
            children: vec![],
        };
        let decoded = round_trip(&orig);
        match &decoded.data {
            Some(NodeData::StaticImage(si)) => {
                assert_eq!(si.fit_mode, fit as i32);
                assert_eq!(si.resource_id.len(), 32);
            }
            _ => panic!("wrong data variant"),
        }
    }
}

#[test]
fn roundtrip_create_tile_mutation() {
    let orig = CreateTileMutation {
        tab_id: b"tab-001".to_vec(),
        bounds: Some(Rect {
            x: 0.0,
            y: 0.0,
            width: 300.0,
            height: 200.0,
        }),
        z_order: 5,
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.tab_id, decoded.tab_id);
    assert_eq!(orig.z_order, decoded.z_order);
    assert_eq!(decoded.bounds.unwrap().width, 300.0);
}

#[test]
fn roundtrip_mutation_proto_all_variants() {
    // CreateTile
    let m1 = MutationProto {
        mutation: Some(Mutation::CreateTile(CreateTileMutation {
            tab_id: b"t".to_vec(),
            bounds: None,
            z_order: 0,
        })),
    };
    let d1 = round_trip(&m1);
    assert!(matches!(d1.mutation, Some(Mutation::CreateTile(_))));

    // SetTileRoot
    let m2 = MutationProto {
        mutation: Some(Mutation::SetTileRoot(SetTileRootMutation {
            tile_id: b"tile-1".to_vec(),
            node: None,
        })),
    };
    let d2 = round_trip(&m2);
    assert!(matches!(d2.mutation, Some(Mutation::SetTileRoot(_))));

    // PublishToZone
    let m3 = MutationProto {
        mutation: Some(Mutation::PublishToZone(PublishToZoneMutation {
            zone_name: "subtitle".to_string(),
            content: Some(ZoneContent {
                payload: Some(ZonePayload::StreamText("hello".to_string())),
            }),
            publish_token: None,
            element_id: Vec::new(),
            merge_key: String::new(),
        })),
    };
    let d3 = round_trip(&m3);
    assert!(matches!(d3.mutation, Some(Mutation::PublishToZone(_))));

    // ClearZone
    let m4 = MutationProto {
        mutation: Some(Mutation::ClearZone(ClearZoneMutation {
            zone_name: "notification".to_string(),
            publish_token: None,
        })),
    };
    let d4 = round_trip(&m4);
    assert!(matches!(d4.mutation, Some(Mutation::ClearZone(_))));

    // ClearWidget
    let m5 = MutationProto {
        mutation: Some(Mutation::ClearWidget(ClearWidgetMutation {
            widget_name: "gauge".to_string(),
            instance_id: String::new(),
        })),
    };
    let d5 = round_trip(&m5);
    assert!(matches!(d5.mutation, Some(Mutation::ClearWidget(_))));
    if let Some(Mutation::ClearWidget(cw)) = d5.mutation {
        assert_eq!(cw.widget_name, "gauge");
    }
}

#[test]
fn roundtrip_zone_content_all_variants() {
    // StreamText
    let z1 = ZoneContent {
        payload: Some(ZonePayload::StreamText("test".to_string())),
    };
    let d1 = round_trip(&z1);
    assert!(matches!(d1.payload, Some(ZonePayload::StreamText(_))));

    // Notification — with actions to catch proto↔scene wiring regressions
    let z2 = ZoneContent {
        payload: Some(ZonePayload::Notification(NotificationPayload {
            text: "Alert!".to_string(),
            icon: "warning.png".to_string(),
            urgency: 2,
            title: String::new(),
            actions: vec![NotificationActionProto {
                label: "Dismiss".to_string(),
                callback_id: "dismiss-1".to_string(),
            }],
        })),
    };
    let d2 = round_trip(&z2);
    match &d2.payload {
        Some(ZonePayload::Notification(n)) => {
            assert_eq!(n.urgency, 2);
            assert_eq!(n.actions.len(), 1);
            assert_eq!(n.actions[0].label, "Dismiss");
            assert_eq!(n.actions[0].callback_id, "dismiss-1");
        }
        _ => panic!("wrong variant"),
    }

    // StatusBar
    let mut entries = std::collections::HashMap::new();
    entries.insert("battery".to_string(), "95%".to_string());
    let z3 = ZoneContent {
        payload: Some(ZonePayload::StatusBar(StatusBarPayload { entries })),
    };
    let d3 = round_trip(&z3);
    match d3.payload {
        Some(ZonePayload::StatusBar(sb)) => assert_eq!(sb.entries["battery"], "95%"),
        _ => panic!("wrong variant"),
    }

    // SolidColor
    let z4 = ZoneContent {
        payload: Some(ZonePayload::SolidColor(Rgba {
            r: 0.1,
            g: 0.2,
            b: 0.3,
            a: 0.4,
        })),
    };
    let d4 = round_trip(&z4);
    assert!(matches!(d4.payload, Some(ZonePayload::SolidColor(_))));
}

#[test]
fn roundtrip_zone_definition_proto() {
    let orig = ZoneDefinitionProto {
        id: "zone-001".to_string(),
        name: "subtitle".to_string(),
        description: "Subtitle display zone".to_string(),
        geometry_policy: Some(GeometryPolicyProto {
            policy: Some(
                tze_hud_protocol::proto::geometry_policy_proto::Policy::Relative(
                    RelativeGeometryPolicy {
                        x_pct: 0.0,
                        y_pct: 0.85,
                        width_pct: 1.0,
                        height_pct: 0.15,
                    },
                ),
            ),
        }),
        accepted_media_types: vec!["text/plain".to_string()],
        rendering_policy: Some(RenderingPolicyProto {
            font_size_px: 16.0,
            backdrop: Some(Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.5,
            }),
            text_align: TextAlignProto::Center as i32,
            margin_px: 8.0,
            // Extended fields: zero-defaults (not set)
            font_family: FontFamilyProto::Unspecified as i32,
            font_weight: 0,
            text_color: None,
            backdrop_opacity: -1.0,
            outline_color: None,
            outline_width: 0.0,
            margin_horizontal: -1.0,
            margin_vertical: -1.0,
            transition_in_ms: 0,
            transition_out_ms: 0,
            overflow: 0,           // TextOverflowProto::Unspecified = not set
            backdrop_radius: -1.0, // -1.0 = not set sentinel
        }),
        contention_policy: ContentionPolicyProto::ContentionPolicyLatestWins as i32,
        max_publishers: 4,
        auto_clear_ms: 5000,
        stack_max_depth: 0,
        merge_max_keys: 0,
        ephemeral: false,
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.name, decoded.name);
    assert_eq!(orig.contention_policy, decoded.contention_policy);
    assert_eq!(orig.max_publishers, decoded.max_publishers);
    assert_eq!(orig.auto_clear_ms, 5000);
}

// ─── events.proto ─────────────────────────────────────────────────────────────

#[test]
fn roundtrip_pointer_move_event() {
    let orig = PointerMoveEvent {
        tile_id: vec![1u8; 16],
        node_id: vec![2u8; 16],
        interaction_id: "btn".to_string(),
        timestamp_mono_us: u64::MAX,
        device_id: "mouse-0".to_string(),
        local_x: 123.456,
        local_y: 789.012,
        display_x: 1920.0,
        display_y: 1080.0,
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.timestamp_mono_us, decoded.timestamp_mono_us);
    assert_eq!(orig.tile_id, decoded.tile_id);
    assert_eq!(decoded.local_x, 123.456);
}

#[test]
fn roundtrip_pointer_down_up_events() {
    let orig_down = PointerDownEvent {
        tile_id: vec![0xAA; 16],
        node_id: vec![0xBB; 16],
        interaction_id: "btn".to_string(),
        timestamp_mono_us: 123_456_789,
        device_id: "mouse-0".to_string(),
        local_x: 50.0,
        local_y: 60.0,
        display_x: 500.0,
        display_y: 600.0,
        button: 0,
    };
    let d_down = round_trip(&orig_down);
    assert_eq!(d_down.button, 0);
    assert_eq!(d_down.timestamp_mono_us, 123_456_789);

    let orig_up = PointerUpEvent {
        tile_id: vec![0xCC; 16],
        button: 1,
        timestamp_mono_us: 999,
        ..Default::default()
    };
    assert_eq!(round_trip(&orig_up).button, 1);
}

#[test]
fn roundtrip_keyboard_events() {
    let orig_down = KeyDownEvent {
        tile_id: vec![0u8; 16],
        node_id: vec![0u8; 16],
        timestamp_mono_us: 5555,
        key_code: "KeyA".to_string(),
        key: "a".to_string(),
        repeat: false,
        ctrl: true,
        shift: false,
        alt: false,
        meta: false,
    };
    let d = round_trip(&orig_down);
    assert_eq!(d.key_code, "KeyA");
    assert!(d.ctrl);
    assert!(!d.shift);

    let orig_up = KeyUpEvent {
        tile_id: vec![0u8; 16],
        node_id: vec![0u8; 16],
        timestamp_mono_us: 6666,
        key_code: "KeyA".to_string(),
        key: "a".to_string(),
        ..Default::default()
    };
    assert_eq!(round_trip(&orig_up).key_code, "KeyA");
}

#[test]
fn roundtrip_focus_events() {
    for &src in &[
        FocusSource::Unspecified,
        FocusSource::Click,
        FocusSource::TabKey,
        FocusSource::Programmatic,
        FocusSource::CommandInput,
    ] {
        let orig = FocusGainedEvent {
            tile_id: vec![0u8; 16],
            node_id: vec![],
            timestamp_mono_us: 42,
            source: src as i32,
        };
        assert_eq!(round_trip(&orig).source, src as i32);
    }

    for &reason in &[
        FocusLostReason::Unspecified,
        FocusLostReason::ClickElsewhere,
        FocusLostReason::TabKey,
        FocusLostReason::Programmatic,
        FocusLostReason::TileDestroyed,
        FocusLostReason::TabSwitched,
        FocusLostReason::LeaseRevoked,
        FocusLostReason::AgentDisconnected,
        FocusLostReason::CommandInput,
    ] {
        let orig = FocusLostEvent {
            tile_id: vec![0u8; 16],
            node_id: vec![],
            timestamp_mono_us: 99,
            reason: reason as i32,
        };
        assert_eq!(round_trip(&orig).reason, reason as i32);
    }
}

#[test]
fn roundtrip_capture_released_event_all_reasons() {
    for &reason in &[
        CaptureReleasedReason::Unspecified,
        CaptureReleasedReason::AgentReleased,
        CaptureReleasedReason::PointerUp,
        CaptureReleasedReason::RuntimeRevoked,
        CaptureReleasedReason::LeaseRevoked,
    ] {
        let orig = CaptureReleasedEvent {
            tile_id: vec![0u8; 16],
            node_id: vec![0u8; 16],
            timestamp_mono_us: 77,
            device_id: "touch-0".to_string(),
            reason: reason as i32,
        };
        assert_eq!(round_trip(&orig).reason, reason as i32);
    }
}

#[test]
fn roundtrip_ime_composition_events() {
    let start = ImeCompositionStartEvent {
        tile_id: vec![0u8; 16],
        node_id: vec![],
        timestamp_mono_us: 1,
    };
    let d_start = round_trip(&start);
    assert_eq!(d_start.timestamp_mono_us, 1);

    let update = ImeCompositionUpdateEvent {
        tile_id: vec![0u8; 16],
        node_id: vec![],
        timestamp_mono_us: 2,
        composition_text: "はよ".to_string(),
    };
    assert_eq!(round_trip(&update).composition_text, "はよ");

    let end = ImeCompositionEndEvent {
        tile_id: vec![0u8; 16],
        node_id: vec![],
        timestamp_mono_us: 3,
        committed_text: "はよ".to_string(),
    };
    assert_eq!(round_trip(&end).committed_text, "はよ");
}

#[test]
fn roundtrip_gesture_and_scroll_events() {
    let gesture = GestureEvent {
        tile_id: vec![0u8; 16],
        node_id: vec![0u8; 16],
        interaction_id: "pinch".to_string(),
        timestamp_mono_us: 12345,
        device_id: "touch-0".to_string(),
        gesture_kind: "pinch".to_string(),
        scale: 1.5,
        rotation: 0.0,
        delta_x: 0.0,
        delta_y: 0.0,
    };
    assert_eq!(round_trip(&gesture).scale, 1.5);

    let scroll = ScrollOffsetChangedEvent {
        tile_id: vec![0u8; 16],
        timestamp_mono_us: 99999,
        offset_x: 0.0,
        offset_y: 150.0,
    };
    assert_eq!(round_trip(&scroll).offset_y, 150.0);
}

#[test]
fn roundtrip_command_input_event_all_actions() {
    for &action in &[
        CommandAction::Unspecified,
        CommandAction::NavigateNext,
        CommandAction::NavigatePrev,
        CommandAction::Activate,
        CommandAction::Cancel,
        CommandAction::Context,
        CommandAction::ScrollUp,
        CommandAction::ScrollDown,
    ] {
        for &source in &[
            CommandSource::Unspecified,
            CommandSource::Keyboard,
            CommandSource::Dpad,
            CommandSource::Voice,
            CommandSource::RemoteClicker,
            CommandSource::RotaryDial,
            CommandSource::Programmatic,
        ] {
            let orig = CommandInputEvent {
                tile_id: vec![0u8; 16],
                node_id: vec![0u8; 16],
                interaction_id: String::new(),
                timestamp_mono_us: 0,
                device_id: String::new(),
                action: action as i32,
                source: source as i32,
            };
            let d = round_trip(&orig);
            assert_eq!(d.action, action as i32);
            assert_eq!(d.source, source as i32);
        }
    }
}

#[test]
fn roundtrip_event_batch() {
    let batch = EventBatch {
        frame_number: 42,
        batch_ts_us: 1_700_000_000_000_000,
        events: vec![
            InputEnvelope {
                event: Some(InputEnvelopeEvent::PointerDown(PointerDownEvent {
                    tile_id: vec![1u8; 16],
                    timestamp_mono_us: 1000,
                    ..Default::default()
                })),
            },
            InputEnvelope {
                event: Some(InputEnvelopeEvent::KeyDown(KeyDownEvent {
                    tile_id: vec![2u8; 16],
                    timestamp_mono_us: 2000,
                    key_code: "KeyB".to_string(),
                    ..Default::default()
                })),
            },
        ],
    };
    let decoded = round_trip(&batch);
    assert_eq!(decoded.frame_number, 42);
    assert_eq!(decoded.events.len(), 2);
}

// ─── session.proto ────────────────────────────────────────────────────────────

#[test]
fn roundtrip_session_init_all_fields() {
    let orig = SessionInit {
        agent_id: "weather-agent".to_string(),
        requested_capabilities: vec![
            "resident_mcp".to_string(),
            "read_scene_topology".to_string(),
        ],
        initial_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
        resume_token: vec![],
        min_protocol_version: 1000,
        max_protocol_version: 1001,
        auth_credential: Some(AuthCredential {
            credential: Some(Credential::PreSharedKey(PreSharedKeyCredential {
                key: "test-key".to_string(),
            })),
        }),
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.agent_id, decoded.agent_id);
    assert_eq!(orig.requested_capabilities, decoded.requested_capabilities);
    assert_eq!(orig.min_protocol_version, decoded.min_protocol_version);
    assert_eq!(orig.max_protocol_version, decoded.max_protocol_version);
    match &decoded.auth_credential {
        Some(ac) => match &ac.credential {
            Some(Credential::PreSharedKey(psk)) => assert_eq!(psk.key, "test-key"),
            _ => panic!("wrong credential variant"),
        },
        None => panic!("missing auth_credential"),
    }
}

#[test]
fn roundtrip_session_init_empty_capabilities() {
    // WHEN SessionInit has empty requested_capabilities THEN valid (no capabilities requested)
    let orig = SessionInit {
        agent_id: "guest-agent".to_string(),
        requested_capabilities: vec![],
        initial_subscriptions: vec![],
        ..Default::default()
    };
    let decoded = round_trip(&orig);
    assert!(decoded.requested_capabilities.is_empty());
    assert_eq!(decoded.agent_id, "guest-agent");
}

#[test]
fn roundtrip_session_resume() {
    let orig = SessionResume {
        agent_id: "weather-agent".to_string(),
        resume_token: vec![
            0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09,
            0x0A, 0x0B,
        ],
        last_seen_server_sequence: 9999,
        pre_shared_key: String::new(),
        auth_credential: Some(AuthCredential {
            credential: Some(Credential::LocalSocket(LocalSocketCredential {
                socket_path: "/run/tze_hud.sock".to_string(),
                pid_hint: "1234".to_string(),
            })),
        }),
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.resume_token, decoded.resume_token);
    assert_eq!(
        orig.last_seen_server_sequence,
        decoded.last_seen_server_sequence
    );
}

#[test]
fn roundtrip_session_established() {
    let orig = SessionEstablished {
        session_id: vec![0u8; 16],
        namespace: "weather-agent".to_string(),
        granted_capabilities: vec!["resident_mcp".to_string()],
        resume_token: vec![0xFF; 16],
        heartbeat_interval_ms: 5000,
        server_sequence: 1,
        compositor_timestamp_wall_us: 1_700_000_000_000_000,
        active_subscriptions: vec![
            "DEGRADATION_NOTICES".to_string(),
            "LEASE_CHANGES".to_string(),
        ],
        denied_subscriptions: vec!["SCENE_TOPOLOGY".to_string()],
        negotiated_protocol_version: 1000,
        portal_part_tokens: Some(ResolvedPortalTokens {
            tokens: std::collections::HashMap::from([
                ("portal.frame.background".to_string(), "#111720".to_string()),
                ("portal.header.font_size".to_string(), "16".to_string()),
            ]),
        }),
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.heartbeat_interval_ms, decoded.heartbeat_interval_ms);
    assert_eq!(orig.server_sequence, decoded.server_sequence);
    assert_eq!(orig.denied_subscriptions, decoded.denied_subscriptions);
    assert_eq!(
        orig.negotiated_protocol_version,
        decoded.negotiated_protocol_version
    );
    // Resolved portal tokens (hud-16um0) survive the wire round-trip.
    assert_eq!(orig.portal_part_tokens, decoded.portal_part_tokens);
    let decoded_tokens = decoded
        .portal_part_tokens
        .expect("portal_part_tokens must round-trip as Some");
    assert_eq!(
        decoded_tokens.tokens.get("portal.frame.background"),
        Some(&"#111720".to_string())
    );
}

#[test]
fn roundtrip_session_error() {
    let orig = SessionError {
        code: "AUTH_FAILED".to_string(),
        message: "Invalid pre-shared key".to_string(),
        hint: "check_psk".to_string(),
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.code, decoded.code);
    assert_eq!(orig.hint, decoded.hint);
}

#[test]
fn roundtrip_session_resume_result() {
    let orig = SessionResumeResult {
        accepted: true,
        new_session_token: vec![0xAB; 16],
        new_server_sequence: 42,
        negotiated_protocol_version: 1001,
        granted_capabilities: vec!["resident_mcp".to_string()],
        active_subscriptions: vec!["DEGRADATION_NOTICES".to_string()],
        denied_subscriptions: vec![],
    };
    let decoded = round_trip(&orig);
    assert!(decoded.accepted);
    assert_eq!(decoded.new_server_sequence, 42);
}

#[test]
fn roundtrip_runtime_error_all_codes() {
    for code in &[
        ErrorCode::Unspecified,
        ErrorCode::Unknown,
        ErrorCode::LeaseExpired,
        ErrorCode::LeaseNotFound,
        ErrorCode::ZoneTypeMismatch,
        ErrorCode::ZoneNotFound,
        ErrorCode::BudgetExceeded,
        ErrorCode::MutationRejected,
        ErrorCode::PermissionDenied,
        ErrorCode::InvalidArgument,
        ErrorCode::SafeModeActive,
        ErrorCode::TimestampTooOld,
        ErrorCode::TimestampTooFuture,
        ErrorCode::TimestampExpiryBeforePresent,
    ] {
        let orig = RuntimeError {
            error_code: format!("{code:?}"),
            message: "test error".to_string(),
            context: "field=value".to_string(),
            hint: "{}".to_string(),
            error_code_enum: *code as i32,
        };
        let decoded = round_trip(&orig);
        assert_eq!(orig.error_code_enum, decoded.error_code_enum);
    }
}

#[test]
fn roundtrip_mutation_batch() {
    let orig = MutationBatch {
        batch_id: vec![0u8; 16],
        lease_id: vec![0xAA; 16],
        mutations: vec![tze_hud_protocol::proto::MutationProto {
            mutation: Some(Mutation::CreateTile(CreateTileMutation {
                tab_id: b"tab-1".to_vec(),
                bounds: Some(Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 100.0,
                }),
                z_order: 1,
            })),
        }],
        timing: Some(TimingHints {
            present_at_wall_us: 1_700_000_000_000_000,
            expires_at_wall_us: 1_700_000_001_000_000,
        }),
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.batch_id, decoded.batch_id);
    assert_eq!(orig.mutations.len(), decoded.mutations.len());
    assert!(decoded.timing.is_some());
}

#[test]
fn roundtrip_lease_request_response() {
    let req = LeaseRequest {
        ttl_ms: 30_000,
        capabilities: vec!["resident_mcp".to_string()],
        lease_priority: 2,
    };
    let d_req = round_trip(&req);
    assert_eq!(d_req.ttl_ms, 30_000);
    assert_eq!(d_req.lease_priority, 2);

    let resp = LeaseResponse {
        granted: true,
        lease_id: vec![0xDE; 16],
        granted_ttl_ms: 30_000,
        granted_priority: 2,
        granted_capabilities: vec!["resident_mcp".to_string()],
        deny_reason: String::new(),
        deny_code: String::new(),
        result: LeaseResult::Granted as i32,
    };
    let d_resp = round_trip(&resp);
    assert!(d_resp.granted);
    assert_eq!(d_resp.granted_ttl_ms, 30_000);
    assert_eq!(d_resp.result, LeaseResult::Granted as i32);

    let denied = LeaseResponse {
        granted: false,
        deny_reason: "budget exceeded".to_string(),
        deny_code: "BUDGET_EXCEEDED".to_string(),
        result: LeaseResult::Denied as i32,
        ..Default::default()
    };
    let d_denied = round_trip(&denied);
    assert!(!d_denied.granted);
    assert_eq!(d_denied.deny_code, "BUDGET_EXCEEDED");
}

#[test]
fn roundtrip_heartbeat() {
    let orig = Heartbeat {
        timestamp_mono_us: u64::MAX,
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.timestamp_mono_us, decoded.timestamp_mono_us);
}

#[test]
fn roundtrip_heartbeat_zero() {
    let orig = Heartbeat {
        timestamp_mono_us: 0,
    };
    let decoded = round_trip(&orig);
    assert_eq!(decoded.timestamp_mono_us, 0);
}

#[test]
fn roundtrip_subscription_change() {
    let orig = SubscriptionChange {
        subscribe: vec!["SCENE_TOPOLOGY".to_string()],
        unsubscribe: vec!["FOCUS_EVENTS".to_string()],
        subscribe_filter: Vec::new(),
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.subscribe, decoded.subscribe);
    assert_eq!(orig.unsubscribe, decoded.unsubscribe);
    assert!(decoded.subscribe_filter.is_empty());
}

#[test]
fn roundtrip_subscription_change_with_filter() {
    // Verify that filter_prefix survives proto encode/decode (RFC 0010 §7.2).
    let orig = SubscriptionChange {
        subscribe: Vec::new(),
        unsubscribe: Vec::new(),
        subscribe_filter: vec![SubscriptionEntry {
            category: "SCENE_TOPOLOGY".to_string(),
            filter_prefix: "scene.zone.".to_string(),
        }],
    };
    let decoded = round_trip(&orig);
    assert_eq!(decoded.subscribe_filter.len(), 1);
    assert_eq!(decoded.subscribe_filter[0].category, "SCENE_TOPOLOGY");
    assert_eq!(decoded.subscribe_filter[0].filter_prefix, "scene.zone.");
}

#[test]
fn roundtrip_client_message_all_payload_variants() {
    // Test a representative sample of ClientMessage payloads
    let msgs = vec![
        ClientMessage {
            sequence: 1,
            timestamp_wall_us: 1_000_000,
            payload: Some(ClientPayload::SessionInit(SessionInit {
                agent_id: "agent-1".to_string(),
                ..Default::default()
            })),
        },
        ClientMessage {
            sequence: 2,
            timestamp_wall_us: 2_000_000,
            payload: Some(ClientPayload::Heartbeat(Heartbeat {
                timestamp_mono_us: 12345,
            })),
        },
        ClientMessage {
            sequence: 3,
            timestamp_wall_us: 3_000_000,
            payload: Some(ClientPayload::MutationBatch(MutationBatch {
                batch_id: vec![0u8; 16],
                lease_id: vec![0u8; 16],
                mutations: vec![],
                timing: None,
            })),
        },
        ClientMessage {
            sequence: 4,
            timestamp_wall_us: 4_000_000,
            payload: Some(ClientPayload::SessionClose(SessionClose {
                reason: "done".to_string(),
            })),
        },
    ];
    for msg in &msgs {
        let decoded = round_trip(msg);
        assert_eq!(msg.sequence, decoded.sequence);
        assert_eq!(msg.timestamp_wall_us, decoded.timestamp_wall_us);
    }
}

#[test]
fn roundtrip_server_message_all_payload_variants() {
    let msgs = vec![
        ServerMessage {
            sequence: 1,
            timestamp_wall_us: 1_000_000,
            payload: Some(ServerPayload::SessionEstablished(
                SessionEstablished::default(),
            )),
        },
        ServerMessage {
            sequence: 2,
            timestamp_wall_us: 2_000_000,
            payload: Some(ServerPayload::SessionError(SessionError {
                code: "AUTH_FAILED".to_string(),
                ..Default::default()
            })),
        },
        ServerMessage {
            sequence: 3,
            timestamp_wall_us: 3_000_000,
            payload: Some(ServerPayload::Heartbeat(Heartbeat {
                timestamp_mono_us: 99,
            })),
        },
    ];
    for msg in &msgs {
        let decoded = round_trip(msg);
        assert_eq!(msg.sequence, decoded.sequence);
    }
}

#[test]
fn roundtrip_scene_snapshot() {
    let orig = SceneSnapshot {
        snapshot_json: r#"{"tabs":[],"tiles":{},"active_tab":""}"#.to_string(),
        sequence: 42,
        snapshot_wall_us: 1_700_000_000_000_000,
        snapshot_mono_us: 5_000_000,
        blake3_checksum: "a".repeat(64),
    };
    let decoded = round_trip(&orig);
    assert_eq!(orig.sequence, decoded.sequence);
    assert_eq!(orig.snapshot_wall_us, decoded.snapshot_wall_us);
    assert_eq!(orig.snapshot_mono_us, decoded.snapshot_mono_us);
    assert_eq!(orig.blake3_checksum, decoded.blake3_checksum);
}

/// Proto3 forward-compatibility: unknown fields are silently ignored by prost.
/// Decoding a message with unknown fields must succeed (not panic/error), and
/// known fields must still be correctly decoded.
///
/// Note: prost does not preserve unknown fields on re-encode (unlike the Go/Java
/// proto3 libraries). This test documents the actual behavior.
#[test]
fn roundtrip_decode_with_unknown_fields_succeeds() {
    // Encode a Heartbeat with timestamp_mono_us = 12345
    let known = Heartbeat {
        timestamp_mono_us: 12345,
    };
    let mut buf = Vec::new();
    known.encode(&mut buf).unwrap();

    // Append a synthetic unknown field: field 9999, wire type 0 (varint), value 42.
    // Proto3 tag encoding: tag = (9999 << 3) | 0 = 79992
    // LEB128 of 79992: 0xF8, 0xF0, 0x04; varint 42 = 0x2A
    buf.extend_from_slice(&[0xF8, 0xF0, 0x04, 0x2A]);

    // Decode MUST succeed even with unknown fields present
    let decoded =
        Heartbeat::decode(buf.as_slice()).expect("decode with unknown fields must not fail");

    // Known fields must be correctly decoded despite the unknown field appended
    assert_eq!(
        decoded.timestamp_mono_us, 12345,
        "known fields must survive decoding alongside unknown fields"
    );
}

// ─── RenderingPolicyProto extended fields (hud-sc0a.2) ───────────────────────

/// Round-trip with ALL extended fields populated (fields 5-15).
#[test]
fn roundtrip_rendering_policy_proto_all_fields_populated() {
    let orig = RenderingPolicyProto {
        // original fields 1-4
        font_size_px: 18.0,
        backdrop: Some(Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.6,
        }),
        text_align: TextAlignProto::Center as i32,
        margin_px: 12.0,
        // extended fields 5-15
        font_family: FontFamilyProto::SystemMonospace as i32,
        font_weight: 700,
        text_color: Some(Rgba {
            r: 1.0,
            g: 0.9,
            b: 0.8,
            a: 1.0,
        }),
        backdrop_opacity: 0.75,
        outline_color: Some(Rgba {
            r: 0.2,
            g: 0.6,
            b: 1.0,
            a: 1.0,
        }),
        outline_width: 2.5,
        margin_horizontal: 16.0,
        margin_vertical: 8.0,
        transition_in_ms: 250,
        transition_out_ms: 150,
        overflow: TextOverflowProto::Ellipsis as i32,
        backdrop_radius: 12.0,
    };
    let decoded = round_trip(&orig);
    // original fields
    assert_eq!(orig.font_size_px, decoded.font_size_px);
    assert_eq!(orig.text_align, decoded.text_align);
    assert_eq!(orig.margin_px, decoded.margin_px);
    assert_eq!(
        orig.backdrop.as_ref().map(|c| c.a),
        decoded.backdrop.as_ref().map(|c| c.a)
    );
    // extended fields
    assert_eq!(orig.font_family, decoded.font_family);
    assert_eq!(orig.font_weight, decoded.font_weight);
    assert_eq!(
        orig.text_color.as_ref().map(|c| c.r),
        decoded.text_color.as_ref().map(|c| c.r)
    );
    assert_eq!(orig.backdrop_opacity, decoded.backdrop_opacity);
    assert_eq!(
        orig.outline_color.as_ref().map(|c| c.r),
        decoded.outline_color.as_ref().map(|c| c.r)
    );
    assert_eq!(orig.outline_width, decoded.outline_width);
    assert_eq!(orig.margin_horizontal, decoded.margin_horizontal);
    assert_eq!(orig.margin_vertical, decoded.margin_vertical);
    assert_eq!(orig.transition_in_ms, decoded.transition_in_ms);
    assert_eq!(orig.transition_out_ms, decoded.transition_out_ms);
    assert_eq!(orig.overflow, decoded.overflow);
    assert_eq!(orig.backdrop_radius, decoded.backdrop_radius);
}

/// Round-trip with all extended fields absent (all-None / zero-value proto defaults).
#[test]
fn roundtrip_rendering_policy_proto_all_fields_none() {
    let orig = RenderingPolicyProto {
        font_size_px: 0.0,
        backdrop: None,
        text_align: TextAlignProto::Unspecified as i32,
        margin_px: 0.0,
        font_family: FontFamilyProto::Unspecified as i32,
        font_weight: 0,
        text_color: None,
        // -1.0 sentinel = not set for backdrop_opacity
        backdrop_opacity: -1.0,
        outline_color: None,
        outline_width: 0.0,
        // -1.0 sentinel = not set for margin_horizontal / margin_vertical
        margin_horizontal: -1.0,
        margin_vertical: -1.0,
        transition_in_ms: 0,
        transition_out_ms: 0,
        overflow: TextOverflowProto::Unspecified as i32,
        // -1.0 sentinel = not set for backdrop_radius
        backdrop_radius: -1.0,
    };
    let decoded = round_trip(&orig);
    assert_eq!(decoded.font_size_px, 0.0);
    assert!(decoded.backdrop.is_none());
    assert_eq!(decoded.font_family, FontFamilyProto::Unspecified as i32);
    assert_eq!(decoded.font_weight, 0);
    assert!(decoded.text_color.is_none());
    assert_eq!(decoded.backdrop_opacity, -1.0);
    assert!(decoded.outline_color.is_none());
    assert_eq!(decoded.outline_width, 0.0);
    assert_eq!(decoded.margin_horizontal, -1.0);
    assert_eq!(decoded.margin_vertical, -1.0);
    assert_eq!(decoded.transition_in_ms, 0);
    assert_eq!(decoded.transition_out_ms, 0);
    assert_eq!(
        decoded.backdrop_radius, -1.0,
        "backdrop_radius absent = -1.0 sentinel"
    );
}

/// Backward-compatibility: a pre-extension serialized RenderingPolicyProto
/// (only fields 1-4) decodes cleanly, and all extended fields default to
/// their "not set" sentinel values (proto3 defaults).
#[test]
fn roundtrip_rendering_policy_proto_backward_compat_pre_extension_format() {
    // Simulate a v0 message that only contains the original 4 fields.
    let pre_extension = RenderingPolicyProto {
        font_size_px: 16.0,
        backdrop: Some(Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.5,
        }),
        text_align: TextAlignProto::Start as i32,
        margin_px: 8.0,
        // All extended fields use proto3 defaults (zero / None)
        font_family: 0,
        font_weight: 0,
        text_color: None,
        backdrop_opacity: 0.0, // proto3 default for float
        outline_color: None,
        outline_width: 0.0,
        margin_horizontal: 0.0,
        margin_vertical: 0.0,
        transition_in_ms: 0,
        transition_out_ms: 0,
        overflow: 0,          // TextOverflowProto::Unspecified = proto3 default
        backdrop_radius: 0.0, // proto3 default for float
    };

    // Encode only the fields that would be present in a v0 wire message.
    // We do this by encoding the full struct (prost skips zero-valued fields in proto3)
    // and then decoding as the current type.
    let mut buf = Vec::new();
    pre_extension.encode(&mut buf).unwrap();
    let decoded =
        RenderingPolicyProto::decode(buf.as_slice()).expect("backward-compat decode must succeed");

    // Original fields must be preserved.
    assert_eq!(decoded.font_size_px, 16.0);
    assert_eq!(decoded.margin_px, 8.0);
    assert_eq!(decoded.text_align, TextAlignProto::Start as i32);
    assert_eq!(decoded.backdrop.as_ref().map(|c| c.a), Some(0.5));

    // Extended fields must be at their proto3 zero defaults.
    assert_eq!(
        decoded.font_family, 0,
        "font_family must default to Unspecified"
    );
    assert_eq!(
        decoded.font_weight, 0,
        "font_weight must default to not-set"
    );
    assert!(decoded.text_color.is_none(), "text_color must be absent");
    assert_eq!(
        decoded.backdrop_opacity, 0.0,
        "backdrop_opacity must default to 0.0"
    );
    assert!(
        decoded.outline_color.is_none(),
        "outline_color must be absent"
    );
    assert_eq!(
        decoded.outline_width, 0.0,
        "outline_width must default to 0.0"
    );
    assert_eq!(
        decoded.transition_in_ms, 0,
        "transition_in_ms must default to 0"
    );
    assert_eq!(
        decoded.transition_out_ms, 0,
        "transition_out_ms must default to 0"
    );
}

/// Round-trip: convert::rendering_policy_to_proto then proto_to_rendering_policy
/// with all 15 fields populated and verify None fields are preserved.
#[test]
fn roundtrip_rendering_policy_convert_all_fields_populated() {
    use tze_hud_protocol::convert::{proto_to_rendering_policy, rendering_policy_to_proto};
    use tze_hud_scene::types::{
        FontFamily, RenderingPolicy, Rgba as SceneRgba, TextAlign, TextOverflow,
    };

    let original = RenderingPolicy {
        font_size_px: Some(20.0),
        backdrop: Some(SceneRgba {
            r: 0.1,
            g: 0.1,
            b: 0.1,
            a: 0.8,
        }),
        text_align: Some(TextAlign::End),
        margin_px: Some(10.0),
        font_family: Some(FontFamily::SystemSerif),
        font_weight: Some(600),
        text_color: Some(SceneRgba {
            r: 1.0,
            g: 1.0,
            b: 0.0,
            a: 1.0,
        }),
        backdrop_opacity: Some(0.5),
        outline_color: Some(SceneRgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        }),
        outline_width: Some(3.0),
        margin_horizontal: Some(12.0),
        margin_vertical: Some(6.0),
        transition_in_ms: Some(300),
        transition_out_ms: Some(200),
        overflow: Some(TextOverflow::Ellipsis),
        key_icon_map: Default::default(),
        backdrop_radius: None,
        stream_tail_anchored: None,
    };

    let proto = rendering_policy_to_proto(&original);
    let recovered = proto_to_rendering_policy(&proto);

    assert_eq!(recovered.font_size_px, original.font_size_px);
    assert_eq!(recovered.text_align, original.text_align);
    assert_eq!(recovered.margin_px, original.margin_px);
    assert_eq!(
        recovered.backdrop.map(|c| c.a),
        original.backdrop.map(|c| c.a)
    );
    assert_eq!(recovered.font_family, original.font_family);
    assert_eq!(recovered.font_weight, original.font_weight);
    assert_eq!(
        recovered.text_color.map(|c| c.r),
        original.text_color.map(|c| c.r)
    );
    assert_eq!(recovered.backdrop_opacity, original.backdrop_opacity);
    assert_eq!(
        recovered.outline_color.map(|c| c.r),
        original.outline_color.map(|c| c.r)
    );
    assert_eq!(recovered.outline_width, original.outline_width);
    assert_eq!(recovered.margin_horizontal, original.margin_horizontal);
    assert_eq!(recovered.margin_vertical, original.margin_vertical);
    assert_eq!(recovered.transition_in_ms, original.transition_in_ms);
    assert_eq!(recovered.transition_out_ms, original.transition_out_ms);
    assert_eq!(recovered.overflow, original.overflow);
}

/// Round-trip: convert with all extended fields as None — original 4 fields survive.
#[test]
fn roundtrip_rendering_policy_convert_all_new_fields_none() {
    use tze_hud_protocol::convert::{proto_to_rendering_policy, rendering_policy_to_proto};
    use tze_hud_scene::types::{RenderingPolicy, Rgba as SceneRgba, TextAlign};

    let original = RenderingPolicy {
        font_size_px: Some(14.0),
        backdrop: Some(SceneRgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.5,
        }),
        text_align: Some(TextAlign::Center),
        margin_px: Some(4.0),
        font_family: None,
        font_weight: None,
        text_color: None,
        backdrop_opacity: None,
        outline_color: None,
        outline_width: None,
        margin_horizontal: None,
        margin_vertical: None,
        transition_in_ms: None,
        transition_out_ms: None,
        overflow: None,
        key_icon_map: Default::default(),
        backdrop_radius: None,
        stream_tail_anchored: None,
    };

    let proto = rendering_policy_to_proto(&original);
    let recovered = proto_to_rendering_policy(&proto);

    // Original fields preserved
    assert_eq!(recovered.font_size_px, original.font_size_px);
    assert_eq!(recovered.text_align, original.text_align);
    assert_eq!(recovered.margin_px, original.margin_px);
    // Extended fields remain None
    assert!(recovered.font_family.is_none());
    assert!(recovered.font_weight.is_none());
    assert!(recovered.text_color.is_none());
    assert!(recovered.backdrop_opacity.is_none());
    assert!(recovered.outline_color.is_none());
    assert!(recovered.outline_width.is_none());
    assert!(recovered.margin_horizontal.is_none());
    assert!(recovered.margin_vertical.is_none());
    assert!(recovered.transition_in_ms.is_none());
    assert!(recovered.transition_out_ms.is_none());
    assert!(recovered.overflow.is_none());
    assert!(recovered.backdrop_radius.is_none());
}

/// Round-trip: backdrop_radius is preserved across proto conversion [hud-ltgk.8].
#[test]
fn roundtrip_rendering_policy_convert_backdrop_radius() {
    use tze_hud_protocol::convert::{proto_to_rendering_policy, rendering_policy_to_proto};
    use tze_hud_scene::types::RenderingPolicy;

    // Some(value) round-trips correctly
    let original = RenderingPolicy {
        backdrop_radius: Some(12.0),
        ..RenderingPolicy::default()
    };
    let proto = rendering_policy_to_proto(&original);
    let recovered = proto_to_rendering_policy(&proto);
    assert_eq!(
        recovered.backdrop_radius,
        Some(12.0),
        "backdrop_radius Some(12.0) must survive proto roundtrip"
    );

    // Zero radius is a valid value and must not be treated as sentinel
    let original_zero = RenderingPolicy {
        backdrop_radius: Some(0.0),
        ..RenderingPolicy::default()
    };
    let proto_zero = rendering_policy_to_proto(&original_zero);
    // Some(0.0) encodes as 0.0 on the wire (not the -1.0 sentinel — that is only used
    // for None). The decode check is `>= 0.0`, and 0.0 >= 0.0 is true, so Some(0.0)
    // survives the roundtrip correctly.
    let recovered_zero = proto_to_rendering_policy(&proto_zero);
    assert_eq!(
        recovered_zero.backdrop_radius,
        Some(0.0),
        "backdrop_radius Some(0.0) must survive proto roundtrip (0.0 >= 0.0 → Some)"
    );

    // None round-trips as None
    let original_none = RenderingPolicy {
        backdrop_radius: None,
        ..RenderingPolicy::default()
    };
    let proto_none = rendering_policy_to_proto(&original_none);
    let recovered_none = proto_to_rendering_policy(&proto_none);
    assert_eq!(
        recovered_none.backdrop_radius, None,
        "backdrop_radius None must survive proto roundtrip"
    );
}

/// Backward-compat: deserialize a pre-extension JSON RenderingPolicy
/// (only original 4 fields) → new extended fields are all None.
#[test]
fn roundtrip_rendering_policy_json_backward_compat_pre_extension() {
    use tze_hud_scene::types::RenderingPolicy;

    // JSON produced by the old schema (only the 4 original fields).
    let old_json = r#"{
        "font_size_px": 16.0,
        "backdrop": {"r": 0.0, "g": 0.0, "b": 0.0, "a": 0.5},
        "text_align": "Center",
        "margin_px": 8.0
    }"#;

    let rp: RenderingPolicy =
        serde_json::from_str(old_json).expect("backward-compat JSON deserialization must succeed");

    assert_eq!(rp.font_size_px, Some(16.0));
    assert_eq!(rp.margin_px, Some(8.0));
    // All extended fields must be None when absent in the JSON
    assert!(
        rp.font_family.is_none(),
        "font_family must be None for pre-extension JSON"
    );
    assert!(
        rp.font_weight.is_none(),
        "font_weight must be None for pre-extension JSON"
    );
    assert!(
        rp.text_color.is_none(),
        "text_color must be None for pre-extension JSON"
    );
    assert!(
        rp.backdrop_opacity.is_none(),
        "backdrop_opacity must be None for pre-extension JSON"
    );
    assert!(
        rp.outline_color.is_none(),
        "outline_color must be None for pre-extension JSON"
    );
    assert!(
        rp.outline_width.is_none(),
        "outline_width must be None for pre-extension JSON"
    );
    assert!(
        rp.margin_horizontal.is_none(),
        "margin_horizontal must be None for pre-extension JSON"
    );
    assert!(
        rp.margin_vertical.is_none(),
        "margin_vertical must be None for pre-extension JSON"
    );
    assert!(
        rp.transition_in_ms.is_none(),
        "transition_in_ms must be None for pre-extension JSON"
    );
    assert!(
        rp.transition_out_ms.is_none(),
        "transition_out_ms must be None for pre-extension JSON"
    );
    assert!(
        rp.overflow.is_none(),
        "overflow must be None for pre-extension JSON"
    );
}

// ─── TextMarkdownNodeProto overflow field (hud-9kt2t) ────────────────────────

/// Proto round-trip: TextMarkdownNodeProto with overflow=Ellipsis survives
/// encode/decode.  Validates field 7 stability.
#[test]
fn roundtrip_text_markdown_node_proto_overflow_ellipsis() {
    let orig = NodeProto {
        layout: 0,
        id: b"node-overflow".to_vec(),
        data: Some(NodeData::TextMarkdown(TextMarkdownNodeProto {
            content: "line one\nline two overflow candidate".to_string(),
            bounds: Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 40.0,
            }),
            font_size_px: 14.0,
            color: Some(Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            }),
            background: None,
            color_runs: vec![],
            overflow: TextOverflowProto::Ellipsis as i32,
        })),
        children: vec![],
    };
    let decoded = round_trip(&orig);
    match &decoded.data {
        Some(NodeData::TextMarkdown(tm)) => {
            assert_eq!(
                tm.overflow,
                TextOverflowProto::Ellipsis as i32,
                "overflow=Ellipsis must survive proto round-trip (field 7)"
            );
        }
        _ => panic!("wrong data variant after round-trip"),
    }
}

/// Proto round-trip: TextMarkdownNodeProto with overflow=Clip survives
/// encode/decode.  Validates that Clip is expressible on the wire.
#[test]
fn roundtrip_text_markdown_node_proto_overflow_clip() {
    let orig = NodeProto {
        layout: 0,
        id: b"node-clip".to_vec(),
        data: Some(NodeData::TextMarkdown(TextMarkdownNodeProto {
            content: "explicit clip content".to_string(),
            bounds: Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 20.0,
            }),
            font_size_px: 12.0,
            color: Some(Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            }),
            background: None,
            color_runs: vec![],
            overflow: TextOverflowProto::Clip as i32,
        })),
        children: vec![],
    };
    let decoded = round_trip(&orig);
    match &decoded.data {
        Some(NodeData::TextMarkdown(tm)) => {
            assert_eq!(
                tm.overflow,
                TextOverflowProto::Clip as i32,
                "overflow=Clip must survive proto round-trip (field 7)"
            );
        }
        _ => panic!("wrong data variant after round-trip"),
    }
}

/// Proto round-trip: TextMarkdownNodeProto omitting overflow (field 7) decodes
/// as Unspecified (proto3 default 0).  Backward-compatibility: old messages
/// that don't set field 7 are still valid.
#[test]
fn roundtrip_text_markdown_node_proto_overflow_absent_defaults_to_unspecified() {
    let orig = NodeProto {
        layout: 0,
        id: b"node-compat".to_vec(),
        data: Some(NodeData::TextMarkdown(TextMarkdownNodeProto {
            content: "old message no overflow field".to_string(),
            bounds: Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 20.0,
            }),
            font_size_px: 12.0,
            color: Some(Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            }),
            background: None,
            color_runs: vec![],
            overflow: 0, // proto3 default = Unspecified
        })),
        children: vec![],
    };
    let decoded = round_trip(&orig);
    match &decoded.data {
        Some(NodeData::TextMarkdown(tm)) => {
            assert_eq!(
                tm.overflow, 0,
                "absent overflow field must decode as 0 (Unspecified)"
            );
        }
        _ => panic!("wrong data variant"),
    }
}

/// Convert round-trip: proto TextMarkdownNodeProto with overflow=Ellipsis
/// converts to a scene TextMarkdownNode with overflow=Ellipsis.  This proves
/// the TruncationCache path is engaged for portal transcript panes.
#[test]
fn convert_text_markdown_node_overflow_ellipsis_proto_to_scene() {
    use tze_hud_protocol::convert::proto_node_to_scene;
    use tze_hud_scene::types::TextOverflow;

    let node_proto = NodeProto {
        layout: 0,
        id: vec![0u8; 16],
        data: Some(NodeData::TextMarkdown(TextMarkdownNodeProto {
            content: "portal transcript text".to_string(),
            bounds: Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 300.0,
                height: 80.0,
            }),
            font_size_px: 14.0,
            color: Some(Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            }),
            background: None,
            color_runs: vec![],
            overflow: TextOverflowProto::Ellipsis as i32,
        })),
        children: vec![],
    };

    let scene_node = proto_node_to_scene(&node_proto)
        .expect("proto_node_to_scene must succeed for a valid TextMarkdownNodeProto");

    match &scene_node.data {
        tze_hud_scene::types::NodeData::TextMarkdown(tm) => {
            assert_eq!(
                tm.overflow,
                TextOverflow::Ellipsis,
                "proto overflow=Ellipsis must convert to scene TextOverflow::Ellipsis"
            );
        }
        _ => panic!("expected TextMarkdown node data"),
    }
}

/// Convert round-trip: proto TextMarkdownNodeProto with overflow=Clip
/// converts to a scene TextMarkdownNode with overflow=Clip.
#[test]
fn convert_text_markdown_node_overflow_clip_proto_to_scene() {
    use tze_hud_protocol::convert::proto_node_to_scene;
    use tze_hud_scene::types::TextOverflow;

    let node_proto = NodeProto {
        layout: 0,
        id: vec![0u8; 16],
        data: Some(NodeData::TextMarkdown(TextMarkdownNodeProto {
            content: "explicit clip text".to_string(),
            bounds: Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 50.0,
            }),
            font_size_px: 12.0,
            color: Some(Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            }),
            background: None,
            color_runs: vec![],
            overflow: TextOverflowProto::Clip as i32,
        })),
        children: vec![],
    };

    let scene_node = proto_node_to_scene(&node_proto).expect("proto_node_to_scene must succeed");

    match &scene_node.data {
        tze_hud_scene::types::NodeData::TextMarkdown(tm) => {
            assert_eq!(
                tm.overflow,
                TextOverflow::Clip,
                "proto overflow=Clip must convert to scene TextOverflow::Clip"
            );
        }
        _ => panic!("expected TextMarkdown node data"),
    }
}

/// Convert round-trip: proto TextMarkdownNodeProto with overflow=Unspecified (0)
/// converts to scene TextOverflow::Ellipsis (the safe default for new nodes).
///
/// This ensures that old messages without field 7 set get Ellipsis behaviour,
/// not Clip, honouring the Phase-1 overflow contract.
#[test]
fn convert_text_markdown_node_overflow_unspecified_defaults_to_ellipsis() {
    use tze_hud_protocol::convert::proto_node_to_scene;
    use tze_hud_scene::types::TextOverflow;

    let node_proto = NodeProto {
        layout: 0,
        id: vec![0u8; 16],
        data: Some(NodeData::TextMarkdown(TextMarkdownNodeProto {
            content: "default overflow text".to_string(),
            bounds: Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 50.0,
            }),
            font_size_px: 12.0,
            color: Some(Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            }),
            background: None,
            color_runs: vec![],
            overflow: 0, // TextOverflowProto::Unspecified — old wire compat
        })),
        children: vec![],
    };

    let scene_node = proto_node_to_scene(&node_proto).expect("proto_node_to_scene must succeed");

    match &scene_node.data {
        tze_hud_scene::types::NodeData::TextMarkdown(tm) => {
            assert_eq!(
                tm.overflow,
                TextOverflow::Ellipsis,
                "Unspecified overflow must default to Ellipsis, not Clip"
            );
        }
        _ => panic!("expected TextMarkdown node data"),
    }
}

/// Convert round-trip: scene TextMarkdownNode with Ellipsis overflow serialises
/// to proto with overflow=Ellipsis and deserialises back to Ellipsis.
///
/// Proves the full scene→proto→scene cycle for field 7.
#[test]
fn convert_text_markdown_node_overflow_scene_to_proto_roundtrip() {
    use tze_hud_protocol::convert::{proto_node_to_scene, scene_node_to_proto};
    use tze_hud_scene::types::{
        FontFamily, Node, NodeData, Rect as SceneRect, Rgba as SceneRgba, SceneId, TextAlign,
        TextMarkdownNode, TextOverflow,
    };

    let scene_node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::TextMarkdown(TextMarkdownNode {
            content: "portal transcript pane".to_string(),
            bounds: SceneRect::new(0.0, 0.0, 300.0, 80.0),
            font_size_px: 14.0,
            font_family: FontFamily::SystemSansSerif,
            color: SceneRgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Ellipsis,
            color_runs: Box::default(),
        }),
    };

    let proto_node = scene_node_to_proto(&scene_node);

    // Check the proto carries Ellipsis on the wire.
    // Use fully qualified proto Data path to avoid shadowing by scene NodeData.
    match &proto_node.data {
        Some(tze_hud_protocol::proto::node_proto::Data::TextMarkdown(tm)) => {
            assert_eq!(
                tm.overflow,
                TextOverflowProto::Ellipsis as i32,
                "scene Ellipsis must serialise to proto Ellipsis (field 7)"
            );
        }
        _ => panic!("expected TextMarkdown data"),
    }

    // Now round-trip back to scene and confirm Ellipsis is preserved
    let recovered = proto_node_to_scene(&proto_node).expect("proto_node_to_scene must succeed");
    match &recovered.data {
        NodeData::TextMarkdown(tm) => {
            assert_eq!(
                tm.overflow,
                TextOverflow::Ellipsis,
                "Ellipsis must survive scene→proto→scene round-trip"
            );
        }
        _ => panic!("expected TextMarkdown node data after round-trip"),
    }
}
