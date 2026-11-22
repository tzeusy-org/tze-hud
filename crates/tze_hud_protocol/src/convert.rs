//! Decode protobuf inputs into scene types.

use crate::proto;
use tze_hud_scene::*;

// ─── Identity round-trips ─────────────────────────────────────────────────────

/// Encode a `SceneId` as a `SceneIdProto` (16 bytes, little-endian).
#[cfg(test)]
fn scene_id_to_proto(id: SceneId) -> proto::SceneIdProto {
    proto::SceneIdProto {
        bytes: id.to_bytes_le().to_vec(),
    }
}

/// Decode a `SceneIdProto` back to a `SceneId`.
///
/// Returns `None` if the `bytes` field is not exactly 16 bytes.
pub fn proto_to_scene_id(p: &proto::SceneIdProto) -> Option<SceneId> {
    SceneId::from_bytes_le(&p.bytes)
}

/// Encode a `ResourceId` as a `ResourceIdProto` (32 raw bytes, never hex).
#[cfg(test)]
fn resource_id_to_proto(id: ResourceId) -> proto::ResourceIdProto {
    proto::ResourceIdProto {
        bytes: id.as_bytes().to_vec(),
    }
}

/// Decode a `ResourceIdProto` back to a `ResourceId`.
///
/// Returns `None` if the `bytes` field is not exactly 32 bytes.
#[cfg(any(test, feature = "dev-mode"))]
pub fn proto_to_resource_id(p: &proto::ResourceIdProto) -> Option<ResourceId> {
    ResourceId::from_slice(&p.bytes)
}

// ─── Colors ───────────────────────────────────────────────────────────────────

/// Convert a protobuf Rgba to a scene Rgba.
pub fn proto_rgba_to_scene(c: &proto::Rgba) -> Rgba {
    Rgba::new(c.r, c.g, c.b, c.a)
}

// ─── Zone conversions ─────────────────────────────────────────────────────────

/// Convert a protobuf ZoneContent to a scene ZoneContent.
pub fn proto_zone_content_to_scene(c: &proto::ZoneContent) -> Option<ZoneContent> {
    use proto::zone_content::Payload;
    match c.payload.as_ref()? {
        Payload::StreamText(s) => Some(ZoneContent::StreamText(s.clone())),
        Payload::Notification(n) => Some(ZoneContent::Notification(NotificationPayload {
            text: n.text.clone(),
            icon: n.icon.clone(),
            urgency: n.urgency,
            // The legacy inner member controls no expiry/fade. Both transports
            // use the top-level Publish TTL/expiry, counted from presentation.
            ttl_ms: None,
            title: n.title.clone(),
            actions: n
                .actions
                .iter()
                .map(|a| NotificationAction {
                    label: a.label.clone(),
                    callback_id: (!a.callback_id.is_empty()).then(|| a.callback_id.clone()),
                    url: (!a.url.is_empty()).then(|| a.url.clone()),
                    role: None,
                })
                .collect(),
        })),
        Payload::StatusBar(sb) => Some(ZoneContent::StatusBar(StatusBarPayload {
            entries: sb.entries.clone(),
        })),
        Payload::SolidColor(c) => Some(ZoneContent::SolidColor(proto_rgba_to_scene(c))),
        // StaticImageRef → ZoneContent::StaticImage (WM-S2b types.proto delta; resource identity).
        // The resource_id carries the 32-byte BLAKE3 hash identifying the uploaded resource.
        Payload::StaticImageRef(r) => {
            if r.resource_id.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&r.resource_id);
                Some(ZoneContent::StaticImage(ResourceId::from_bytes(arr)))
            } else {
                // Malformed resource_id: treat as absent (fail-fast on decode would
                // require a Result return; callers rely on None = "skip").
                None
            }
        }
    }
}

/// Convert a scene GeometryPolicy to a protobuf GeometryPolicyProto.
pub fn geometry_policy_to_proto(gp: &GeometryPolicy) -> proto::GeometryPolicyProto {
    use proto::geometry_policy_proto::Policy;
    let policy = match gp {
        GeometryPolicy::Relative {
            x_pct,
            y_pct,
            width_pct,
            height_pct,
        } => Policy::Relative(proto::RelativeGeometryPolicy {
            x_pct: *x_pct,
            y_pct: *y_pct,
            width_pct: *width_pct,
            height_pct: *height_pct,
        }),
        GeometryPolicy::EdgeAnchored {
            edge,
            height_pct,
            width_pct,
            margin_px,
        } => {
            let edge_proto = match edge {
                DisplayEdge::Top => proto::DisplayEdge::Top,
                DisplayEdge::Bottom => proto::DisplayEdge::Bottom,
                DisplayEdge::Left => proto::DisplayEdge::Left,
                DisplayEdge::Right => proto::DisplayEdge::Right,
            };
            Policy::EdgeAnchored(proto::EdgeAnchoredGeometryPolicy {
                edge: edge_proto as i32,
                height_pct: *height_pct,
                width_pct: *width_pct,
                margin_px: *margin_px,
            })
        }
    };
    proto::GeometryPolicyProto {
        policy: Some(policy),
    }
}

// ─── Widget conversions ───────────────────────────────────────────────────────

/// Convert a proto WidgetParameterValueProto to a scene (name, WidgetParameterValue) pair.
///
/// Returns `None` if the value variant is absent.
pub fn proto_to_widget_param_value(
    p: &proto::WidgetParameterValueProto,
) -> Option<(String, WidgetParameterValue)> {
    use proto::widget_parameter_value_proto::Value;
    let value = match p.value.as_ref()? {
        Value::DeadlineMs(ms) => {
            WidgetParameterValue::Deadline(tze_hud_scene::WidgetDeadline::relative(*ms))
        }
        Value::F32Value(f) => WidgetParameterValue::F32(*f),
        Value::StringValue(s) => WidgetParameterValue::String(s.clone()),
        Value::ColorValue(c) => WidgetParameterValue::Color(Rgba::new(c.r, c.g, c.b, c.a)),
        Value::EnumValue(e) => WidgetParameterValue::Enum(e.clone()),
    };
    Some((p.param_name.clone(), value))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SceneId / ResourceId proto round-trips ───────────────────────────────

    #[test]
    fn scene_id_proto_round_trip() {
        let id = SceneId::new();
        let proto = scene_id_to_proto(id);
        assert_eq!(proto.bytes.len(), 16, "SceneId proto must be 16 bytes");
        let restored = proto_to_scene_id(&proto).expect("must decode 16 bytes");
        assert_eq!(id, restored, "SceneId proto round-trip must be lossless");
    }

    #[test]
    fn scene_id_null_proto_round_trip() {
        let null = SceneId::null();
        let proto = scene_id_to_proto(null);
        assert_eq!(proto.bytes, vec![0u8; 16]);
        let restored = proto_to_scene_id(&proto).unwrap();
        assert!(restored.is_null());
    }

    #[test]
    fn scene_id_proto_rejects_wrong_length() {
        let bad = crate::proto::SceneIdProto {
            bytes: vec![0u8; 15],
        };
        assert!(proto_to_scene_id(&bad).is_none());
    }

    #[test]
    fn resource_id_proto_round_trip() {
        let id = ResourceId::of(b"proto round-trip test content");
        let proto = resource_id_to_proto(id);
        assert_eq!(proto.bytes.len(), 32, "ResourceId proto must be 32 bytes");
        let restored = proto_to_resource_id(&proto).expect("must decode 32 bytes");
        assert_eq!(id, restored, "ResourceId proto round-trip must be lossless");
    }

    #[test]
    fn resource_id_proto_bytes_are_raw_not_hex() {
        let id = ResourceId::of(b"no hex on the wire");
        let proto = resource_id_to_proto(id);
        // The bytes field must not be a hex string — verify it has exactly 32 bytes
        // and that it matches the raw hash bytes.
        assert_eq!(&proto.bytes[..], id.as_bytes());
    }

    #[test]
    fn resource_id_proto_rejects_wrong_length() {
        let bad = crate::proto::ResourceIdProto {
            bytes: vec![0u8; 31],
        };
        assert!(proto_to_resource_id(&bad).is_none());
    }

    // ── WidgetParameterValue wire mapping ─────────────────────────────────────

    #[test]
    fn widget_param_value_wire_mapping() {
        use proto::widget_parameter_value_proto::Value;
        let mk = |v| proto::WidgetParameterValueProto {
            param_name: "p".to_string(),
            value: Some(v),
        };
        assert_eq!(
            proto_to_widget_param_value(&mk(Value::F32Value(0.5))),
            Some(("p".to_string(), WidgetParameterValue::F32(0.5)))
        );
        assert_eq!(
            proto_to_widget_param_value(&mk(Value::ColorValue(proto::Rgba {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                a: 1.0
            }))),
            Some((
                "p".to_string(),
                WidgetParameterValue::Color(Rgba::new(1.0, 0.0, 0.0, 1.0))
            ))
        );
        // An absent value variant is dropped, not defaulted.
        let empty = proto::WidgetParameterValueProto {
            param_name: "p".to_string(),
            value: None,
        };
        assert_eq!(proto_to_widget_param_value(&empty), None);
    }
}
