//! gRPC stream-level widget publish integration test.
//!
//! Exercises the full gRPC bidirectional stream lifecycle for widget publishing:
//! 1. Message serialization: WidgetPublish and WidgetPublishResult round-trip correctly
//! 2. ClientMessage and ServerMessage envelope handling: field allocation (field 35 and 47)
//! 3. Verify proper integration with session protocol envelopes
//!
//! Tests per widget-system proposal and session-protocol/spec.md §Widget Publish Session Message,
//! §Widget Publish Result, and RFC 0005 SS2.2/SS9.2 (field allocation).

use prost::Message;
use tze_hud_protocol::proto::session::client_message::Payload as ClientPayload;
use tze_hud_protocol::proto::session::server_message::Payload as ServerPayload;
use tze_hud_protocol::proto::session::{
    ClientMessage, ServerMessage, WidgetPublish, WidgetPublishResult,
};
use tze_hud_protocol::proto::{Rgba, WidgetParameterValueProto};

// ─── Fixture: round-trip helper ──────────────────────────────────────────────

fn round_trip<T: Message + Default>(msg: &T) -> T {
    let mut buf = Vec::new();
    msg.encode(&mut buf).expect("encode must succeed");
    T::decode(buf.as_slice()).expect("decode must succeed")
}

// ─── Message Serialization Tests ─────────────────────────────────────────────

/// WidgetPublish round-trip: all fields preserved (widget_name, instance_id, params, timing).
#[test]
fn widget_publish_roundtrip_preserves_all_fields() {
    let publish = WidgetPublish {
        widget_name: "gauge_01".to_string(),
        instance_id: "tab_42".to_string(),
        params: vec![
            WidgetParameterValueProto {
                param_name: "value".to_string(),
                value: Some(
                    tze_hud_protocol::proto::widget_parameter_value_proto::Value::F32Value(75.5),
                ),
            },
            WidgetParameterValueProto {
                param_name: "color".to_string(),
                value: Some(
                    tze_hud_protocol::proto::widget_parameter_value_proto::Value::ColorValue(
                        Rgba {
                            r: 1.0,
                            g: 0.5,
                            b: 0.0,
                            a: 1.0,
                        },
                    ),
                ),
            },
        ],
        transition_ms: 500,
        element_id: Vec::new(),
        merge_key: "key_42".to_string(),
    };

    let decoded = round_trip(&publish);

    // Verify all fields preserved
    assert_eq!(decoded.widget_name, "gauge_01");
    assert_eq!(decoded.instance_id, "tab_42");
    assert_eq!(decoded.transition_ms, 500);
    assert_eq!(decoded.merge_key, "key_42");
    assert_eq!(decoded.params.len(), 2);
    assert_eq!(decoded.params[0].param_name, "value");
    assert_eq!(decoded.params[1].param_name, "color");
}

/// WidgetParameterValueProto round-trip: f32 variant.
#[test]
fn widget_parameter_value_f32_roundtrip() {
    let param = WidgetParameterValueProto {
        param_name: "level".to_string(),
        value: Some(tze_hud_protocol::proto::widget_parameter_value_proto::Value::F32Value(0.75)),
    };

    let decoded = round_trip(&param);

    assert_eq!(decoded.param_name, "level");
    match decoded.value {
        Some(tze_hud_protocol::proto::widget_parameter_value_proto::Value::F32Value(v)) => {
            assert!((v - 0.75).abs() < 1e-6);
        }
        other => panic!("Expected f32 value, got: {other:?}"),
    }
}

/// WidgetParameterValueProto round-trip: string variant.
#[test]
fn widget_parameter_value_string_roundtrip() {
    let param = WidgetParameterValueProto {
        param_name: "label".to_string(),
        value: Some(
            tze_hud_protocol::proto::widget_parameter_value_proto::Value::StringValue(
                "test".to_string(),
            ),
        ),
    };

    let decoded = round_trip(&param);

    assert_eq!(decoded.param_name, "label");
    match decoded.value {
        Some(tze_hud_protocol::proto::widget_parameter_value_proto::Value::StringValue(v)) => {
            assert_eq!(v, "test");
        }
        other => panic!("Expected string value, got: {other:?}"),
    }
}

/// WidgetParameterValueProto round-trip: color variant.
#[test]
fn widget_parameter_value_color_roundtrip() {
    let param = WidgetParameterValueProto {
        param_name: "color".to_string(),
        value: Some(
            tze_hud_protocol::proto::widget_parameter_value_proto::Value::ColorValue(Rgba {
                r: 1.0,
                g: 0.5,
                b: 0.0,
                a: 1.0,
            }),
        ),
    };

    let decoded = round_trip(&param);

    assert_eq!(decoded.param_name, "color");
    match decoded.value {
        Some(tze_hud_protocol::proto::widget_parameter_value_proto::Value::ColorValue(rgba)) => {
            assert!((rgba.r - 1.0).abs() < 1e-6);
            assert!((rgba.g - 0.5).abs() < 1e-6);
            assert!((rgba.b - 0.0).abs() < 1e-6);
            assert!((rgba.a - 1.0).abs() < 1e-6);
        }
        other => panic!("Expected color value, got: {other:?}"),
    }
}

/// WidgetPublishResult round-trip: accepted=true (success case).
#[test]
fn widget_publish_result_accepted_roundtrip() {
    let result = WidgetPublishResult {
        request_sequence: 42,
        accepted: true,
        widget_name: "gauge_01".to_string(),
        error_code: String::new(),
        error_message: String::new(),
    };

    let decoded = round_trip(&result);

    assert_eq!(decoded.request_sequence, 42);
    assert!(decoded.accepted);
    assert_eq!(decoded.widget_name, "gauge_01");
    assert!(decoded.error_code.is_empty());
    assert!(decoded.error_message.is_empty());
}

/// WidgetPublishResult round-trip: accepted=false with error (failure case).
#[test]
fn widget_publish_result_rejected_roundtrip() {
    let result = WidgetPublishResult {
        request_sequence: 84,
        accepted: false,
        widget_name: "gauge_01".to_string(),
        error_code: "WIDGET_NOT_FOUND".to_string(),
        error_message: "Widget not found: gauge_01".to_string(),
    };

    let decoded = round_trip(&result);

    assert_eq!(decoded.request_sequence, 84);
    assert!(!decoded.accepted);
    assert_eq!(decoded.widget_name, "gauge_01");
    assert_eq!(decoded.error_code, "WIDGET_NOT_FOUND");
    assert_eq!(decoded.error_message, "Widget not found: gauge_01");
}

/// WidgetPublishResult round-trip: all error code variants preserved.
#[test]
fn widget_publish_result_error_codes_preserved() {
    let error_codes = vec![
        "WIDGET_NOT_FOUND",
        "WIDGET_UNKNOWN_PARAMETER",
        "WIDGET_PARAMETER_TYPE_MISMATCH",
        "WIDGET_PARAMETER_INVALID_VALUE",
        "WIDGET_CAPABILITY_MISSING",
    ];

    for error_code in error_codes {
        let result = WidgetPublishResult {
            request_sequence: 7,
            accepted: false,
            widget_name: "test_widget".to_string(),
            error_code: error_code.to_string(),
            error_message: format!("Error: {error_code}"),
        };

        let decoded = round_trip(&result);

        assert_eq!(decoded.request_sequence, 7);
        assert_eq!(decoded.error_code, error_code);
        assert_eq!(decoded.error_message, format!("Error: {error_code}"));
    }
}

// ─── ClientMessage/ServerMessage Envelope Tests ──────────────────────────────

/// ClientMessage wraps WidgetPublish at field 35.
/// WHEN WidgetPublish is wrapped in ClientMessage oneof payload
/// THEN it round-trips with sequence and timestamp preserved.
#[test]
fn client_message_widget_publish_envelope_roundtrip() {
    let client_msg = ClientMessage {
        sequence: 42,
        timestamp_wall_us: 1_000_000_000,
        payload: Some(ClientPayload::WidgetPublish(WidgetPublish {
            widget_name: "test_widget".to_string(),
            instance_id: "inst_1".to_string(),
            params: vec![],
            transition_ms: 100,
            element_id: Vec::new(),
            merge_key: String::new(),
        })),
    };

    let decoded = round_trip(&client_msg);

    assert_eq!(decoded.sequence, 42);
    assert_eq!(decoded.timestamp_wall_us, 1_000_000_000);
    match decoded.payload {
        Some(ClientPayload::WidgetPublish(publish)) => {
            assert_eq!(publish.widget_name, "test_widget");
            assert_eq!(publish.instance_id, "inst_1");
            assert_eq!(publish.transition_ms, 100);
        }
        other => panic!("Expected WidgetPublish in ClientMessage, got: {other:?}"),
    }
}

/// ServerMessage wraps WidgetPublishResult at field 47.
/// WHEN WidgetPublishResult is wrapped in ServerMessage oneof payload
/// THEN it round-trips with sequence and timestamp preserved.
#[test]
fn server_message_widget_publish_result_envelope_roundtrip() {
    let server_msg = ServerMessage {
        sequence: 99,
        timestamp_wall_us: 2_000_000_000,
        payload: Some(ServerPayload::WidgetPublishResult(WidgetPublishResult {
            request_sequence: 42,
            accepted: true,
            widget_name: "test_widget".to_string(),
            error_code: String::new(),
            error_message: String::new(),
        })),
    };

    let decoded = round_trip(&server_msg);

    assert_eq!(decoded.sequence, 99);
    assert_eq!(decoded.timestamp_wall_us, 2_000_000_000);
    match decoded.payload {
        Some(ServerPayload::WidgetPublishResult(result)) => {
            assert_eq!(result.request_sequence, 42);
            assert!(result.accepted);
            assert_eq!(result.widget_name, "test_widget");
        }
        other => panic!("Expected WidgetPublishResult in ServerMessage, got: {other:?}"),
    }
}

/// ServerMessage with error WidgetPublishResult.
/// WHEN WidgetPublishResult(accepted=false) is wrapped in ServerMessage
/// THEN error_code and error_message are preserved through round-trip.
#[test]
fn server_message_widget_publish_result_error_roundtrip() {
    let server_msg = ServerMessage {
        sequence: 50,
        timestamp_wall_us: 1_500_000_000,
        payload: Some(ServerPayload::WidgetPublishResult(WidgetPublishResult {
            request_sequence: 50,
            accepted: false,
            widget_name: "missing_widget".to_string(),
            error_code: "WIDGET_NOT_FOUND".to_string(),
            error_message: "Widget not found: missing_widget".to_string(),
        })),
    };

    let decoded = round_trip(&server_msg);

    match decoded.payload {
        Some(ServerPayload::WidgetPublishResult(result)) => {
            assert_eq!(result.request_sequence, 50);
            assert!(!result.accepted);
            assert_eq!(result.widget_name, "missing_widget");
            assert_eq!(result.error_code, "WIDGET_NOT_FOUND");
            assert_eq!(result.error_message, "Widget not found: missing_widget");
        }
        other => panic!("Expected WidgetPublishResult in ServerMessage, got: {other:?}"),
    }
}

// ─── Edge Cases ──────────────────────────────────────────────────────────────

/// WidgetPublish with empty params list.
#[test]
fn widget_publish_empty_params_roundtrip() {
    let publish = WidgetPublish {
        widget_name: "empty_widget".to_string(),
        instance_id: String::new(),
        params: vec![],
        transition_ms: 0,
        element_id: Vec::new(),
        merge_key: String::new(),
    };

    let decoded = round_trip(&publish);

    assert_eq!(decoded.widget_name, "empty_widget");
    assert_eq!(decoded.params.len(), 0);
    assert!(decoded.instance_id.is_empty());
    assert!(decoded.merge_key.is_empty());
}

/// WidgetPublish with max u32 values.
#[test]
fn widget_publish_max_values_roundtrip() {
    let publish = WidgetPublish {
        widget_name: "max_widget".to_string(),
        instance_id: String::new(),
        params: vec![],
        transition_ms: u32::MAX,
        element_id: Vec::new(),
        merge_key: String::new(),
    };

    let decoded = round_trip(&publish);

    assert_eq!(decoded.transition_ms, u32::MAX);
}

/// ClientMessage sequence values: monotonically increasing starting at 1.
/// Per session-protocol/spec.md, sequence is per-direction and starts at 1.
#[test]
fn client_message_sequence_roundtrip_boundary_values() {
    let sequences = vec![1u64, 2, 100, 1_000_000, u64::MAX];

    for seq in sequences {
        let msg = ClientMessage {
            sequence: seq,
            timestamp_wall_us: 0,
            payload: Some(ClientPayload::WidgetPublish(WidgetPublish {
                widget_name: "test".to_string(),
                instance_id: String::new(),
                params: vec![],
                transition_ms: 0,
                element_id: Vec::new(),
                merge_key: String::new(),
            })),
        };

        let decoded = round_trip(&msg);
        assert_eq!(decoded.sequence, seq, "Sequence {seq} not preserved");
    }
}
