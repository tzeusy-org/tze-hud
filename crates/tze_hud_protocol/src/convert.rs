//! Decode protobuf inputs into scene types and encode scene fixtures.
//!
//! Scene-node encoders are test-only fixtures.

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

// ─── Geometry ─────────────────────────────────────────────────────────────────

/// Convert a protobuf Rect to a scene Rect.
pub fn proto_rect_to_scene(r: &proto::Rect) -> Rect {
    Rect::new(r.x, r.y, r.width, r.height)
}

/// Convert a protobuf Rgba to a scene Rgba.
pub fn proto_rgba_to_scene(c: &proto::Rgba) -> Rgba {
    Rgba::new(c.r, c.g, c.b, c.a)
}

/// Convert a protobuf `HitRegionNodeProto` to a scene [`HitRegionNode`].
///
/// Used by node-tree conversion, including composer-input flags. Missing
/// `bounds` use the same default as other node geometry.
pub fn proto_hit_region_to_scene(hr: &proto::HitRegionNodeProto) -> HitRegionNode {
    let bounds = hr
        .bounds
        .as_ref()
        .map(proto_rect_to_scene)
        .unwrap_or(Rect::new(0.0, 0.0, 100.0, 50.0));
    HitRegionNode {
        bounds,
        interaction_id: hr.interaction_id.clone(),
        accepts_focus: hr.accepts_focus,
        accepts_pointer: hr.accepts_pointer,
        auto_capture: hr.auto_capture,
        release_on_up: hr.release_on_up,
        accepts_composer_input: false,
        ..Default::default()
    }
}

/// Convert a protobuf NodeProto to a scene Node.
pub fn proto_node_to_scene(n: &proto::NodeProto) -> Option<Node> {
    let id = if n.id.is_empty() {
        SceneId::new()
    } else {
        // Decode 16-byte little-endian UUIDv7 SceneId from bytes field.
        // Treat the null sentinel (16 zero bytes) and invalid lengths as absent
        // to avoid introducing a null ID into the live node map.
        match SceneId::from_bytes_le(&n.id) {
            Some(decoded) if decoded == SceneId::null() => SceneId::new(),
            Some(decoded) => decoded,
            None => SceneId::new(),
        }
    };

    let data = match &n.data {
        Some(proto::node_proto::Data::SolidColor(sc)) => {
            let color = sc
                .color
                .as_ref()
                .map(proto_rgba_to_scene)
                .unwrap_or(Rgba::WHITE);
            let bounds = sc
                .bounds
                .as_ref()
                .map(proto_rect_to_scene)
                .unwrap_or(Rect::new(0.0, 0.0, 100.0, 100.0));
            NodeData::SolidColor(SolidColorNode {
                color,
                bounds,
                radius: if sc.radius >= 0.0 {
                    Some(sc.radius)
                } else {
                    None
                },
            })
        }
        Some(proto::node_proto::Data::TextMarkdown(tm)) => {
            let color = tm
                .color
                .as_ref()
                .map(proto_rgba_to_scene)
                .unwrap_or(Rgba::WHITE);
            let bg = tm.background.as_ref().map(proto_rgba_to_scene);
            let bounds = tm
                .bounds
                .as_ref()
                .map(proto_rect_to_scene)
                .unwrap_or(Rect::new(0.0, 0.0, 100.0, 100.0));
            let color_runs = proto_color_runs_to_scene(&tm.color_runs);
            let font_family =
                if proto_text_markdown_uses_literal_full_span(&tm.content, color, &color_runs) {
                    FontFamily::SystemMonospace
                } else {
                    FontFamily::SystemSansSerif
                };
            let overflow = proto_text_overflow_to_scene(tm.overflow);
            NodeData::TextMarkdown(TextMarkdownNode {
                content: tm.content.clone(),
                bounds,
                font_size_px: if tm.font_size_px > 0.0 {
                    tm.font_size_px
                } else {
                    16.0
                },
                font_family,
                color,
                background: bg,
                alignment: TextAlign::Start,
                overflow,
                color_runs,
            })
        }
        Some(proto::node_proto::Data::HitRegion(hr)) => {
            NodeData::HitRegion(proto_hit_region_to_scene(hr))
        }
        Some(proto::node_proto::Data::StaticImage(si)) => {
            let bounds = si
                .bounds
                .as_ref()
                .map(proto_rect_to_scene)
                .unwrap_or(Rect::new(0.0, 0.0, 100.0, 100.0));
            // Unknown wire values are coerced to Unspecified (treated as Contain).
            // Warn so protocol mismatches surface in logs rather than failing silently.
            let fit_mode_proto =
                proto::ImageFitModeProto::try_from(si.fit_mode).unwrap_or_else(|_| {
                    tracing::warn!(
                        raw_value = si.fit_mode,
                        "unknown ImageFitModeProto wire value; defaulting to Unspecified (Contain)"
                    );
                    proto::ImageFitModeProto::ImageFitModeUnspecified
                });
            let fit_mode = match fit_mode_proto {
                proto::ImageFitModeProto::ImageFitModeContain
                | proto::ImageFitModeProto::ImageFitModeUnspecified => ImageFitMode::Contain,
                proto::ImageFitModeProto::ImageFitModeCover => ImageFitMode::Cover,
                proto::ImageFitModeProto::ImageFitModeFill => ImageFitMode::Fill,
                proto::ImageFitModeProto::ImageFitModeScaleDown => ImageFitMode::ScaleDown,
            };
            // RS-4: resource_id is 32 raw bytes on the wire (NOT hex-encoded).
            // Reject nodes with malformed resource_id (wrong length = protocol violation).
            let resource_id = ResourceId::from_slice(&si.resource_id)?;
            // decoded_bytes is runtime-owned metadata for budget accounting.
            // Do not trust client-supplied values; the runtime populates this
            // from the resource store record when processing the mutation.
            NodeData::StaticImage(StaticImageNode {
                resource_id,
                width: si.width,
                height: si.height,
                decoded_bytes: 0,
                fit_mode,
                bounds,
            })
        }
        None => return None,
    };

    Some(Node {
        layout: proto_node_layout_to_scene(n.layout),
        id,
        children: vec![],
        data,
    })
}

/// Map a `NodeLayoutProto` wire value to the scene [`NodeLayout`]. Unknown and
/// UNSPECIFIED values coerce to `Absolute` (the byte-compatible default), so an
/// old publisher that never sets the field renders exactly as before.
pub fn proto_node_layout_to_scene(v: i32) -> NodeLayout {
    let layout_proto = proto::NodeLayoutProto::try_from(v).unwrap_or_else(|_| {
        tracing::warn!(
            raw_value = v,
            "unknown NodeLayoutProto wire value; defaulting to Unspecified (Absolute)"
        );
        proto::NodeLayoutProto::Unspecified
    });
    match layout_proto {
        proto::NodeLayoutProto::VerticalFlow => NodeLayout::VerticalFlow,
        proto::NodeLayoutProto::Absolute | proto::NodeLayoutProto::Unspecified => {
            NodeLayout::Absolute
        }
    }
}

/// Map a scene [`NodeLayout`] to its `NodeLayoutProto` wire value. `Absolute`
/// emits UNSPECIFIED (0) so a flat/absolute node stays byte-identical to the
/// pre-layout wire.
#[cfg(test)]
fn scene_node_layout_to_proto(layout: NodeLayout) -> i32 {
    match layout {
        NodeLayout::Absolute => proto::NodeLayoutProto::Unspecified as i32,
        NodeLayout::VerticalFlow => proto::NodeLayoutProto::VerticalFlow as i32,
    }
}

/// Recursively convert a `NodeProto` (with optional inline `children`,
/// hud-ga4md) into a FLAT list of scene [`Node`]s, ROOT FIRST, with every
/// node's `children` field populated with its direct children's assigned
/// [`SceneId`]s.
///
/// The scene graph stores nodes in a flat id→node map where each node
/// references its children by id, so the natural materialization payload for an
/// inline subtree is this flat list: element `[0]` is the root and every other
/// element is a descendant (any depth). Feed it to
/// [`SceneGraph::set_tile_root_tree`]/[`SceneGraph::set_tile_root_tree_checked`],
/// which insert the whole subtree atomically as one mutation.
///
/// A childless `NodeProto` yields a single-element `Vec` whose sole node is
/// exactly what [`proto_node_to_scene`] returns — so existing flat publishers
/// stay byte-for-byte identical. Returns `None` only if the ROOT proto has no
/// decodable `data`; an individual child that fails to decode (missing `data`
/// or malformed resource id) is skipped rather than collapsing the whole tree,
/// mirroring [`proto_node_to_scene`]'s per-node `Option` semantics.
pub fn proto_node_tree_to_scene(n: &proto::NodeProto) -> Option<Vec<Node>> {
    // Shallow-convert this node first (children field left empty by
    // proto_node_to_scene); its id is assigned here and referenced by the parent.
    let mut root = proto_node_to_scene(n)?;
    let mut flat: Vec<Node> = Vec::new();
    for child_proto in &n.children {
        if let Some(sub) = proto_node_tree_to_scene(child_proto) {
            // `sub` is non-empty by construction (root always present), so [0] is
            // the child's root — link the parent to it and splice the child's
            // whole flattened subtree into ours.
            root.children.push(sub[0].id);
            flat.extend(sub);
        }
    }
    let mut out = Vec::with_capacity(1 + flat.len());
    out.push(root);
    out.extend(flat);
    Some(out)
}

/// Convert the `oneof data` from an `UpdateNodeContentMutation` proto to a
/// scene `NodeData`.  Returns `None` if the variant is missing or malformed.
pub fn proto_update_node_content_data_to_scene(
    d: &proto::update_node_content_mutation::Data,
) -> Option<NodeData> {
    use proto::update_node_content_mutation::Data;
    match d {
        Data::SolidColor(sc) => {
            let color = sc
                .color
                .as_ref()
                .map(proto_rgba_to_scene)
                .unwrap_or(Rgba::WHITE);
            let bounds = sc
                .bounds
                .as_ref()
                .map(proto_rect_to_scene)
                .unwrap_or(Rect::new(0.0, 0.0, 100.0, 100.0));
            Some(NodeData::SolidColor(SolidColorNode {
                color,
                bounds,
                radius: if sc.radius >= 0.0 {
                    Some(sc.radius)
                } else {
                    None
                },
            }))
        }
        Data::TextMarkdown(tm) => {
            let color = tm
                .color
                .as_ref()
                .map(proto_rgba_to_scene)
                .unwrap_or(Rgba::WHITE);
            let bg = tm.background.as_ref().map(proto_rgba_to_scene);
            let bounds = tm
                .bounds
                .as_ref()
                .map(proto_rect_to_scene)
                .unwrap_or(Rect::new(0.0, 0.0, 100.0, 100.0));
            let color_runs = proto_color_runs_to_scene(&tm.color_runs);
            let font_family =
                if proto_text_markdown_uses_literal_full_span(&tm.content, color, &color_runs) {
                    FontFamily::SystemMonospace
                } else {
                    FontFamily::SystemSansSerif
                };
            let overflow = proto_text_overflow_to_scene(tm.overflow);
            Some(NodeData::TextMarkdown(TextMarkdownNode {
                content: tm.content.clone(),
                bounds,
                font_size_px: if tm.font_size_px > 0.0 {
                    tm.font_size_px
                } else {
                    16.0
                },
                font_family,
                color,
                background: bg,
                alignment: TextAlign::Start,
                overflow,
                color_runs,
            }))
        }
        Data::HitRegion(hr) => {
            let bounds = hr
                .bounds
                .as_ref()
                .map(proto_rect_to_scene)
                .unwrap_or(Rect::new(0.0, 0.0, 100.0, 50.0));
            Some(NodeData::HitRegion(HitRegionNode {
                bounds,
                interaction_id: hr.interaction_id.clone(),
                accepts_focus: hr.accepts_focus,
                accepts_pointer: hr.accepts_pointer,
                auto_capture: hr.auto_capture,
                release_on_up: hr.release_on_up,
                accepts_composer_input: false,
                ..Default::default()
            }))
        }
        Data::StaticImage(si) => {
            let bounds = si
                .bounds
                .as_ref()
                .map(proto_rect_to_scene)
                .unwrap_or(Rect::new(0.0, 0.0, 100.0, 100.0));
            // Unknown wire values are coerced to Unspecified (treated as Contain).
            // Warn so protocol mismatches surface in logs rather than failing silently.
            let fit_mode_proto =
                proto::ImageFitModeProto::try_from(si.fit_mode).unwrap_or_else(|_| {
                    tracing::warn!(
                        raw_value = si.fit_mode,
                        "unknown ImageFitModeProto wire value; defaulting to Unspecified (Contain)"
                    );
                    proto::ImageFitModeProto::ImageFitModeUnspecified
                });
            let fit_mode = match fit_mode_proto {
                proto::ImageFitModeProto::ImageFitModeContain
                | proto::ImageFitModeProto::ImageFitModeUnspecified => ImageFitMode::Contain,
                proto::ImageFitModeProto::ImageFitModeCover => ImageFitMode::Cover,
                proto::ImageFitModeProto::ImageFitModeFill => ImageFitMode::Fill,
                proto::ImageFitModeProto::ImageFitModeScaleDown => ImageFitMode::ScaleDown,
            };
            let resource_id = ResourceId::from_slice(&si.resource_id)?;
            Some(NodeData::StaticImage(StaticImageNode {
                resource_id,
                width: si.width,
                height: si.height,
                decoded_bytes: 0,
                fit_mode,
                bounds,
            }))
        }
    }
}

// ─── Color-run conversions ────────────────────────────────────────────────────

/// Convert a slice of `TextColorRunProto` to a `Box<[TextColorRun]>`.
///
/// Malformed runs (missing `color` field) are converted with an opaque white
/// fallback rather than being dropped, so callers can detect unexpected proto
/// states via invariant validation rather than silent loss.
pub fn proto_color_runs_to_scene(runs: &[proto::TextColorRunProto]) -> Box<[TextColorRun]> {
    runs.iter()
        .map(|r| TextColorRun {
            start_byte: r.start_byte,
            end_byte: r.end_byte,
            color: r
                .color
                .as_ref()
                .map(proto_rgba_to_scene)
                .unwrap_or(Rgba::WHITE),
        })
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

/// The v1 TextMarkdown proto does not yet expose `font_family` directly.
///
/// The text-stream portal composer sends one full-span color run identical to
/// the base color to request literal raw text rendering (markdown markers must
/// stay visible).  Treat that narrow wire shape as an editor/composer text
/// surface and render it in monospace so caret positioning can stay stable.
fn proto_text_markdown_uses_literal_full_span(
    content: &str,
    color: Rgba,
    runs: &[TextColorRun],
) -> bool {
    let [run] = runs else {
        return false;
    };
    run.start_byte == 0 && run.end_byte as usize == content.len() && run.color == color
}

/// Decode a `TextOverflowProto` i32 (field 7 of `TextMarkdownNodeProto`) to the
/// scene `TextOverflow`.
///
/// `UNSPECIFIED` (proto default `0`) maps to `Ellipsis` so that newly-written
/// callers that omit the field get the correct overflow contract, and so that
/// live portal transcript panes explicitly setting `Ellipsis` engage the
/// `TruncationCache` path.  Callers that genuinely want clip must send
/// `TEXT_OVERFLOW_PROTO_CLIP` explicitly.
pub fn proto_text_overflow_to_scene(v: i32) -> TextOverflow {
    // Unknown wire values are coerced to Unspecified (treated as Ellipsis).
    // Warn so protocol mismatches surface in logs rather than failing silently.
    let overflow_proto = proto::TextOverflowProto::try_from(v).unwrap_or_else(|_| {
        tracing::warn!(
            raw_value = v,
            "unknown TextOverflowProto wire value; defaulting to Unspecified (Ellipsis)"
        );
        proto::TextOverflowProto::Unspecified
    });
    match overflow_proto {
        proto::TextOverflowProto::Clip => TextOverflow::Clip,
        proto::TextOverflowProto::Ellipsis | proto::TextOverflowProto::Unspecified => {
            TextOverflow::Ellipsis
        }
    }
}

/// Convert a scene `TextOverflow` to the `TextOverflowProto` i32 used in
/// `TextMarkdownNodeProto::overflow` (field 7).
#[cfg(test)]
fn scene_text_overflow_to_proto(ov: TextOverflow) -> i32 {
    match ov {
        TextOverflow::Clip => proto::TextOverflowProto::Clip as i32,
        TextOverflow::Ellipsis => proto::TextOverflowProto::Ellipsis as i32,
    }
}

/// Convert a `Box<[TextColorRun]>` to a `Vec<TextColorRunProto>`.
#[cfg(test)]
fn scene_color_runs_to_proto(runs: &[TextColorRun]) -> Vec<proto::TextColorRunProto> {
    runs.iter()
        .map(|r| proto::TextColorRunProto {
            start_byte: r.start_byte,
            end_byte: r.end_byte,
            color: Some(proto::Rgba {
                r: r.color.r,
                g: r.color.g,
                b: r.color.b,
                a: r.color.a,
            }),
        })
        .collect()
}

// ─── Mutation conversions ─────────────────────────────────────────────────────

/// Convert a `TileInputModeProto` to the scene `InputMode`.
pub fn proto_input_mode_to_scene(m: proto::TileInputModeProto) -> InputMode {
    match m {
        proto::TileInputModeProto::TileInputModePassthrough => InputMode::Passthrough,
        proto::TileInputModeProto::TileInputModeCapture
        | proto::TileInputModeProto::TileInputModeUnspecified => InputMode::Capture,
        proto::TileInputModeProto::TileInputModeLocalOnly => InputMode::LocalOnly,
    }
}

/// Convert a scene `InputMode` to a `TileInputModeProto`.
#[cfg(test)]
fn scene_input_mode_to_proto(m: InputMode) -> proto::TileInputModeProto {
    match m {
        InputMode::Passthrough => proto::TileInputModeProto::TileInputModePassthrough,
        InputMode::Capture => proto::TileInputModeProto::TileInputModeCapture,
        InputMode::LocalOnly => proto::TileInputModeProto::TileInputModeLocalOnly,
    }
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
                    callback_id: a.callback_id.clone(),
                })
                .collect(),
        })),
        Payload::StatusBar(sb) => Some(ZoneContent::StatusBar(StatusBarPayload {
            entries: sb.entries.clone(),
        })),
        Payload::SolidColor(c) => Some(ZoneContent::SolidColor(proto_rgba_to_scene(c))),
        // StaticImageRef → ZoneContent::StaticImage (WM-S2b types.proto delta; RFC 0011 resource identity).
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

/// Convert a scene Node to a protobuf NodeProto.
#[cfg(test)]
pub(crate) fn scene_node_to_proto(n: &Node) -> proto::NodeProto {
    let data = match &n.data {
        NodeData::SolidColor(sc) => Some(proto::node_proto::Data::SolidColor(
            proto::SolidColorNodeProto {
                color: Some(proto::Rgba {
                    r: sc.color.r,
                    g: sc.color.g,
                    b: sc.color.b,
                    a: sc.color.a,
                }),
                bounds: Some(proto::Rect {
                    x: sc.bounds.x,
                    y: sc.bounds.y,
                    width: sc.bounds.width,
                    height: sc.bounds.height,
                }),
                radius: sc.radius.unwrap_or(-1.0),
            },
        )),
        NodeData::TextMarkdown(tm) => Some(proto::node_proto::Data::TextMarkdown(
            proto::TextMarkdownNodeProto {
                content: tm.content.clone(),
                bounds: Some(proto::Rect {
                    x: tm.bounds.x,
                    y: tm.bounds.y,
                    width: tm.bounds.width,
                    height: tm.bounds.height,
                }),
                font_size_px: tm.font_size_px,
                color: Some(proto::Rgba {
                    r: tm.color.r,
                    g: tm.color.g,
                    b: tm.color.b,
                    a: tm.color.a,
                }),
                background: tm.background.map(|c| proto::Rgba {
                    r: c.r,
                    g: c.g,
                    b: c.b,
                    a: c.a,
                }),
                color_runs: scene_color_runs_to_proto(&tm.color_runs),
                overflow: scene_text_overflow_to_proto(tm.overflow),
            },
        )),
        NodeData::HitRegion(hr) => Some(proto::node_proto::Data::HitRegion(
            proto::HitRegionNodeProto {
                bounds: Some(proto::Rect {
                    x: hr.bounds.x,
                    y: hr.bounds.y,
                    width: hr.bounds.width,
                    height: hr.bounds.height,
                }),
                interaction_id: hr.interaction_id.clone(),
                accepts_focus: hr.accepts_focus,
                accepts_pointer: hr.accepts_pointer,
                auto_capture: hr.auto_capture,
                release_on_up: hr.release_on_up,
            },
        )),
        NodeData::StaticImage(si) => {
            let fit_mode = match si.fit_mode {
                ImageFitMode::Contain => proto::ImageFitModeProto::ImageFitModeContain as i32,
                ImageFitMode::Cover => proto::ImageFitModeProto::ImageFitModeCover as i32,
                ImageFitMode::Fill => proto::ImageFitModeProto::ImageFitModeFill as i32,
                ImageFitMode::ScaleDown => proto::ImageFitModeProto::ImageFitModeScaleDown as i32,
            };
            Some(proto::node_proto::Data::StaticImage(
                proto::StaticImageNodeProto {
                    // RS-4: wire format is 32 raw bytes (not hex).
                    resource_id: si.resource_id.as_bytes().to_vec(),
                    width: si.width,
                    height: si.height,
                    decoded_bytes: si.decoded_bytes,
                    fit_mode,
                    bounds: Some(proto::Rect {
                        x: si.bounds.x,
                        y: si.bounds.y,
                        width: si.bounds.width,
                        height: si.bounds.height,
                    }),
                },
            ))
        }
    };
    proto::NodeProto {
        id: n.id.to_bytes_le().to_vec(),
        data,
        // Flat scene->proto: a bare scene `Node` carries only child SceneId refs,
        // which cannot be resolved to child structs without the graph, so the
        // inline-children field is left empty (hud-ga4md). Use
        // [`scene_node_tree_to_proto`] with a node lookup to emit a nested subtree.
        children: vec![],
        layout: scene_node_layout_to_proto(n.layout),
    }
}

/// Convert a scene `Node` and its descendants into a nested `NodeProto` with
/// inline `children` (hud-ga4md) — the reverse of [`proto_node_tree_to_scene`].
///
/// `root.children` are resolved against `lookup` (a flat id→node map, e.g. a
/// `SceneGraph`'s node map) and emitted recursively as `NodeProto.children`. A
/// child id absent from `lookup` is skipped (a dangling ref never aborts the
/// walk). A leaf root (`children` empty) produces exactly what
/// [`scene_node_to_proto`] returns, so round-tripping a flat node is
/// byte-identical.
#[cfg(test)]
fn scene_node_tree_to_proto(
    root: &Node,
    lookup: &std::collections::HashMap<SceneId, Node>,
) -> proto::NodeProto {
    let mut proto = scene_node_to_proto(root);
    proto.children = root
        .children
        .iter()
        .filter_map(|child_id| lookup.get(child_id))
        .map(|child| scene_node_tree_to_proto(child, lookup))
        .collect();
    proto
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

    // RS-4: StaticImageNode uses resource_id + decoded_bytes; no raw blob.
    fn make_static_image_node(fit_mode: ImageFitMode) -> Node {
        let resource_id = ResourceId::of(b"4x4 test image resource");
        Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::StaticImage(StaticImageNode {
                resource_id,
                width: 4,
                height: 4,
                decoded_bytes: 4 * 4 * 4u64, // 4×4 RGBA8
                fit_mode,
                bounds: Rect::new(10.0, 20.0, 80.0, 60.0),
            }),
        }
    }

    #[test]
    fn test_static_image_proto_roundtrip_contain() {
        let original = make_static_image_node(ImageFitMode::Contain);
        let proto = scene_node_to_proto(&original);
        let restored = proto_node_to_scene(&proto).expect("conversion must succeed");

        if let NodeData::StaticImage(si) = &restored.data {
            assert_eq!(si.width, 4);
            assert_eq!(si.height, 4);
            // decoded_bytes is runtime-owned metadata; proto_node_to_scene zeroes it out
            // (client-supplied values are not trusted — the runtime populates this
            // from the resource store after mutation).
            assert_eq!(
                si.decoded_bytes, 0,
                "decoded_bytes must be zeroed on ingestion (runtime sets this, not the client)"
            );
            assert_eq!(si.fit_mode, ImageFitMode::Contain);
            assert_eq!(si.bounds.x, 10.0);
            assert_eq!(si.bounds.y, 20.0);
            // resource_id must survive proto roundtrip as 32 raw bytes.
            let original_id = ResourceId::of(b"4x4 test image resource");
            assert_eq!(
                si.resource_id, original_id,
                "resource_id must be preserved across proto roundtrip"
            );
        } else {
            panic!("expected StaticImage variant after proto roundtrip");
        }
    }

    #[test]
    fn test_static_image_proto_roundtrip_all_fit_modes() {
        for (fit_mode, label) in [
            (ImageFitMode::Contain, "Contain"),
            (ImageFitMode::Cover, "Cover"),
            (ImageFitMode::Fill, "Fill"),
            (ImageFitMode::ScaleDown, "ScaleDown"),
        ] {
            let original = make_static_image_node(fit_mode);
            let proto = scene_node_to_proto(&original);
            let restored = proto_node_to_scene(&proto)
                .unwrap_or_else(|| panic!("conversion failed for {label}"));
            if let NodeData::StaticImage(si) = &restored.data {
                assert_eq!(si.fit_mode, fit_mode, "fit_mode mismatch for {label}");
            } else {
                panic!("wrong variant for {label}");
            }
        }
    }

    #[test]
    fn test_static_image_proto_preserves_resource_id() {
        // RS-4: Verify ResourceId (32 bytes) survives proto encode/decode as raw bytes.
        let resource_id = ResourceId::of(b"some unique resource bytes for testing");
        let node = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::StaticImage(StaticImageNode {
                resource_id,
                width: 4,
                height: 1,
                decoded_bytes: 4 * 4u64, // 4×1 RGBA8
                fit_mode: ImageFitMode::Fill,
                bounds: Rect::new(0.0, 0.0, 100.0, 25.0),
            }),
        };

        let proto = scene_node_to_proto(&node);

        // The wire must carry raw 32 bytes (not hex).
        if let Some(crate::proto::node_proto::Data::StaticImage(ref p)) = proto.data {
            assert_eq!(
                p.resource_id.len(),
                32,
                "wire format must be 32 raw bytes (RS-4: not hex)"
            );
            assert_eq!(
                &p.resource_id[..],
                resource_id.as_bytes(),
                "wire bytes must match the raw BLAKE3 digest"
            );
        }

        let restored = proto_node_to_scene(&proto).unwrap();
        if let NodeData::StaticImage(si) = &restored.data {
            assert_eq!(
                si.resource_id, resource_id,
                "ResourceId must survive proto roundtrip"
            );
        } else {
            panic!("wrong variant");
        }
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

    // ── InputMode proto round-trips ───────────────────────────────────────────

    #[test]
    fn input_mode_passthrough_round_trip() {
        let mode = InputMode::Passthrough;
        let proto = scene_input_mode_to_proto(mode);
        assert_eq!(
            proto,
            crate::proto::TileInputModeProto::TileInputModePassthrough
        );
        let restored = proto_input_mode_to_scene(proto);
        assert_eq!(restored, mode);
    }

    #[test]
    fn input_mode_capture_round_trip() {
        let mode = InputMode::Capture;
        let proto = scene_input_mode_to_proto(mode);
        assert_eq!(
            proto,
            crate::proto::TileInputModeProto::TileInputModeCapture
        );
        let restored = proto_input_mode_to_scene(proto);
        assert_eq!(restored, mode);
    }

    #[test]
    fn input_mode_local_only_round_trip() {
        let mode = InputMode::LocalOnly;
        let proto = scene_input_mode_to_proto(mode);
        assert_eq!(
            proto,
            crate::proto::TileInputModeProto::TileInputModeLocalOnly
        );
        let restored = proto_input_mode_to_scene(proto);
        assert_eq!(restored, mode);
    }

    #[test]
    fn input_mode_unspecified_maps_to_capture() {
        // UNSPECIFIED (0) must default to Capture for forward-compat.
        let restored =
            proto_input_mode_to_scene(crate::proto::TileInputModeProto::TileInputModeUnspecified);
        assert_eq!(
            restored,
            InputMode::Capture,
            "UNSPECIFIED input mode must map to Capture (safe default)"
        );
    }

    /// Retired wire field 7 cannot enable local-only native composer input.
    #[test]
    fn hit_region_wire_cannot_enable_local_composer() {
        use prost::Message;
        let wire = proto::HitRegionNodeProto {
            bounds: Some(proto::Rect {
                x: 1.0,
                y: 2.0,
                width: 30.0,
                height: 40.0,
            }),
            interaction_id: "ordinary-region".into(),
            accepts_focus: true,
            accepts_pointer: true,
            auto_capture: true,
            release_on_up: true,
        };
        for legacy_flag in [false, true] {
            let mut bytes = wire.encode_to_vec();
            if legacy_flag {
                bytes.extend_from_slice(&[0x38, 1]);
            }
            let decoded = proto::HitRegionNodeProto::decode(bytes.as_slice()).unwrap();
            assert_eq!(decoded, wire);
            let node = proto::NodeProto {
                layout: 0,
                id: Vec::new(),
                children: vec![],
                data: Some(proto::node_proto::Data::HitRegion(decoded.clone())),
            };
            let mut scene_node = proto_node_to_scene(&node).unwrap();
            let direct = proto_hit_region_to_scene(&decoded);
            let NodeData::HitRegion(region) = &mut scene_node.data else {
                panic!("expected hit region")
            };
            assert!(!region.accepts_composer_input);
            assert!(!direct.accepts_composer_input);
            assert_eq!(region.bounds, direct.bounds);
            assert_eq!(region.interaction_id, "ordinary-region");
            assert!(
                region.accepts_focus
                    && region.accepts_pointer
                    && region.auto_capture
                    && region.release_on_up
            );
            assert_eq!(direct.interaction_id, region.interaction_id);
            assert!(
                direct.accepts_focus
                    && direct.accepts_pointer
                    && direct.auto_capture
                    && direct.release_on_up
            );
            let update = proto::update_node_content_mutation::Data::HitRegion(decoded.clone());
            let NodeData::HitRegion(updated) =
                proto_update_node_content_data_to_scene(&update).unwrap()
            else {
                panic!("expected updated hit region")
            };
            assert!(!updated.accepts_composer_input);
            assert_eq!(updated.bounds, Rect::new(1.0, 2.0, 30.0, 40.0));
            assert_eq!(updated.interaction_id, "ordinary-region");
            assert!(
                updated.accepts_focus
                    && updated.accepts_pointer
                    && updated.auto_capture
                    && updated.release_on_up
            );
            // Even a native composer is encoded as an ordinary wire region.
            region.accepts_composer_input = true;
            let encoded = scene_node_to_proto(&scene_node);
            let Some(proto::node_proto::Data::HitRegion(encoded)) = encoded.data else {
                panic!("expected hit region")
            };
            assert_eq!(encoded, wire);
        }
    }

    // ── Inline NodeProto.children subtree round-trips (hud-ga4md) ─────────────

    fn text_proto(id: &[u8], content: &str, children: Vec<proto::NodeProto>) -> proto::NodeProto {
        proto::NodeProto {
            layout: 0,
            id: id.to_vec(),
            data: Some(proto::node_proto::Data::TextMarkdown(
                proto::TextMarkdownNodeProto {
                    content: content.to_string(),
                    bounds: Some(proto::Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 100.0,
                        height: 20.0,
                    }),
                    font_size_px: 16.0,
                    color: None,
                    background: None,
                    color_runs: vec![],
                    overflow: proto::TextOverflowProto::Unspecified as i32,
                },
            )),
            children,
        }
    }

    #[test]
    fn empty_children_flat_conversion_matches_proto_node_to_scene() {
        // Byte-compatibility guarantee: a childless NodeProto through the
        // subtree converter yields exactly the single flat node the legacy
        // scalar converter produces (same id, same data, empty children).
        let p = text_proto(&SceneId::new().to_bytes_le(), "hello", vec![]);
        let flat = proto_node_to_scene(&p).expect("flat convert");
        let tree = proto_node_tree_to_scene(&p).expect("tree convert");
        assert_eq!(tree.len(), 1, "childless proto → single-node list");
        assert_eq!(tree[0].id, flat.id);
        assert!(tree[0].children.is_empty());
        assert_eq!(tree[0].data, flat.data);
    }

    #[test]
    fn nested_children_flatten_root_first_with_linked_ids() {
        // root → [a → [grandchild], b]
        let gid = SceneId::new().to_bytes_le();
        let aid = SceneId::new().to_bytes_le();
        let bid = SceneId::new().to_bytes_le();
        let rid = SceneId::new().to_bytes_le();
        let grandchild = text_proto(&gid, "grandchild", vec![]);
        let a = text_proto(&aid, "a", vec![grandchild]);
        let b = text_proto(&bid, "b", vec![]);
        let root = text_proto(&rid, "root", vec![a, b]);

        let flat = proto_node_tree_to_scene(&root).expect("tree convert");
        assert_eq!(flat.len(), 4, "root + a + grandchild + b");
        // Root is element 0 and links to a and b (in wire order).
        let root_scene = &flat[0];
        assert_eq!(root_scene.id, SceneId::from_bytes_le(&rid).unwrap());
        assert_eq!(
            root_scene.children,
            vec![
                SceneId::from_bytes_le(&aid).unwrap(),
                SceneId::from_bytes_le(&bid).unwrap()
            ]
        );
        // `a` links to its grandchild.
        let a_scene = flat
            .iter()
            .find(|n| n.id == SceneId::from_bytes_le(&aid).unwrap())
            .expect("a present");
        assert_eq!(
            a_scene.children,
            vec![SceneId::from_bytes_le(&gid).unwrap()]
        );
    }

    #[test]
    fn proto_scene_proto_round_trip_preserves_nested_children() {
        // proto (with inline children) → flat scene subtree → nested proto.
        // Explicit ids so encode/decode is a pure identity (no server assignment).
        let gid = SceneId::new().to_bytes_le();
        let aid = SceneId::new().to_bytes_le();
        let bid = SceneId::new().to_bytes_le();
        let rid = SceneId::new().to_bytes_le();
        let orig = text_proto(
            &rid,
            "root",
            vec![
                text_proto(&aid, "a", vec![text_proto(&gid, "grandchild", vec![])]),
                text_proto(&bid, "b", vec![]),
            ],
        );

        let flat = proto_node_tree_to_scene(&orig).expect("tree convert");
        let lookup: std::collections::HashMap<SceneId, Node> =
            flat.iter().cloned().map(|n| (n.id, n)).collect();
        let rebuilt = scene_node_tree_to_proto(&flat[0], &lookup);

        // Structure survives: root id, two children, grandchild under first child.
        assert_eq!(rebuilt.id, orig.id);
        assert_eq!(rebuilt.children.len(), 2);
        assert_eq!(rebuilt.children[0].id, aid.to_vec());
        assert_eq!(rebuilt.children[1].id, bid.to_vec());
        assert_eq!(rebuilt.children[0].children.len(), 1);
        assert_eq!(rebuilt.children[0].children[0].id, gid.to_vec());
        // Leaf content survives the whole trip.
        match &rebuilt.children[0].children[0].data {
            Some(proto::node_proto::Data::TextMarkdown(tm)) => {
                assert_eq!(tm.content, "grandchild");
            }
            other => panic!("expected TextMarkdown grandchild, got {other:?}"),
        }
    }

    /// The additive `NodeLayout` field round-trips proto ↔ scene, with Absolute
    /// re-emitting UNSPECIFIED (0) so a flat/absolute node stays byte-identical to
    /// the pre-layout wire, and unknown wire values coercing to Absolute
    /// (hud-yfj8u).
    #[test]
    fn node_layout_round_trips_proto_scene_proto() {
        let rid = SceneId::new().to_bytes_le();
        let mut node = text_proto(&rid, "flat", vec![]);

        // An unset proto layout is Absolute, and re-emits as UNSPECIFIED (0).
        assert_eq!(
            node.layout, 0,
            "text_proto builds an unset (Absolute) layout"
        );
        let scene_abs = proto_node_to_scene(&node).expect("convert");
        assert_eq!(scene_abs.layout, NodeLayout::Absolute);
        assert_eq!(
            scene_node_to_proto(&scene_abs).layout,
            0,
            "Absolute re-emits UNSPECIFIED so the flat wire is byte-compatible"
        );

        // VERTICAL_FLOW survives proto → scene → proto.
        node.layout = proto::NodeLayoutProto::VerticalFlow as i32;
        let scene_flow = proto_node_to_scene(&node).expect("convert");
        assert_eq!(scene_flow.layout, NodeLayout::VerticalFlow);
        assert_eq!(
            scene_node_to_proto(&scene_flow).layout,
            proto::NodeLayoutProto::VerticalFlow as i32
        );

        // Explicit ABSOLUTE(1) also maps to scene Absolute.
        node.layout = proto::NodeLayoutProto::Absolute as i32;
        assert_eq!(
            proto_node_to_scene(&node).expect("convert").layout,
            NodeLayout::Absolute
        );

        // Unknown wire value coerces to Absolute rather than panicking.
        node.layout = 999;
        assert_eq!(
            proto_node_to_scene(&node).expect("convert").layout,
            NodeLayout::Absolute
        );
    }

    // ── Hand-written mapper coverage (moved from the deleted roundtrip.rs) ──

    #[test]
    fn color_runs_round_trip_proto_scene_proto() {
        let proto_runs = vec![
            proto::TextColorRunProto {
                start_byte: 0,
                end_byte: 5,
                color: Some(proto::Rgba {
                    r: 1.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                }),
            },
            proto::TextColorRunProto {
                start_byte: 7,
                end_byte: 16,
                color: Some(proto::Rgba {
                    r: 0.0,
                    g: 1.0,
                    b: 0.0,
                    a: 1.0,
                }),
            },
        ];
        let scene_runs = proto_color_runs_to_scene(&proto_runs);
        assert_eq!(scene_runs.len(), 2);
        assert_eq!((scene_runs[1].start_byte, scene_runs[1].end_byte), (7, 16));
        assert_eq!(scene_color_runs_to_proto(&scene_runs), proto_runs);
    }

    #[test]
    fn text_overflow_wire_mapping() {
        // Unspecified (old wire messages) and unknown values default to Ellipsis, not Clip.
        for (wire, want) in [
            (
                proto::TextOverflowProto::Unspecified as i32,
                TextOverflow::Ellipsis,
            ),
            (
                proto::TextOverflowProto::Ellipsis as i32,
                TextOverflow::Ellipsis,
            ),
            (proto::TextOverflowProto::Clip as i32, TextOverflow::Clip),
            (999, TextOverflow::Ellipsis),
        ] {
            assert_eq!(proto_text_overflow_to_scene(wire), want, "wire {wire}");
        }
        for ov in [TextOverflow::Ellipsis, TextOverflow::Clip] {
            assert_eq!(
                proto_text_overflow_to_scene(scene_text_overflow_to_proto(ov)),
                ov
            );
        }
    }
}
