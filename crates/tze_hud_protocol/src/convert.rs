//! Conversion between protobuf types and scene graph types.

use crate::proto;
use tze_hud_scene::*;

// ─── Identity round-trips ─────────────────────────────────────────────────────

/// Encode a `SceneId` as a `SceneIdProto` (16 bytes, little-endian).
pub fn scene_id_to_proto(id: SceneId) -> proto::SceneIdProto {
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
pub fn resource_id_to_proto(id: ResourceId) -> proto::ResourceIdProto {
    proto::ResourceIdProto {
        bytes: id.as_bytes().to_vec(),
    }
}

/// Decode a `ResourceIdProto` back to a `ResourceId`.
///
/// Returns `None` if the `bytes` field is not exactly 32 bytes.
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
/// Shared by the node-tree conversion ([`proto_node_to_scene`]) and the
/// `SetTileComposerInteraction` composer-spec decode (session-server + in-process
/// apply paths, hud-iofav), so a composer hit region carried as coalescible
/// overlay state decodes identically to one carried in a node tree. Missing
/// `bounds` fall back to the same default as the node path.
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
        accepts_composer_input: hr.accepts_composer_input,
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
pub fn scene_node_layout_to_proto(layout: NodeLayout) -> i32 {
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
                accepts_composer_input: hr.accepts_composer_input,
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
pub fn scene_text_overflow_to_proto(ov: TextOverflow) -> i32 {
    match ov {
        TextOverflow::Clip => proto::TextOverflowProto::Clip as i32,
        TextOverflow::Ellipsis => proto::TextOverflowProto::Ellipsis as i32,
    }
}

/// Convert a `Box<[TextColorRun]>` to a `Vec<TextColorRunProto>`.
pub fn scene_color_runs_to_proto(runs: &[TextColorRun]) -> Vec<proto::TextColorRunProto> {
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
pub fn scene_input_mode_to_proto(m: InputMode) -> proto::TileInputModeProto {
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
            // ttl_ms is intentionally None on the gRPC path: the protobuf
            // NotificationPayload does not yet carry a ttl_ms field.
            // Per-notification TTL override is currently MCP-only.
            // To support it over gRPC, add the field to types.proto and
            // round-trip it in both directions here.
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
pub fn scene_node_to_proto(n: &Node) -> proto::NodeProto {
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
                accepts_composer_input: hr.accepts_composer_input,
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
pub fn scene_node_tree_to_proto(
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

// ─── In-process portal render-batch apply ──────────────────────────────────────

/// Apply a portal-content [`proto::session::MutationBatch`] directly to a
/// [`SceneGraph`] for the in-process cooperative projection driver.
///
/// The gRPC/wire portal family renders content by *sending*
/// `ResidentGrpcPortalAdapter::render_batch`'s output over the session stream,
/// where the session server converts it and calls [`SceneGraph::apply_batch`].
/// The in-process driver owns the [`SceneGraph`] directly and already knows the
/// real tile [`SceneId`] (returned from its own `create_tile`) and the owning
/// `namespace`, so it applies the equivalent scene mutations here — no
/// element-store lookup, lease decode, or tile-id byte round-trip required. This
/// is the missing render step that left cooperative projections painting an
/// empty grey tile (hud-utbiy): the driver created and tracked the tile but
/// never turned `render_batch` content into scene nodes.
///
/// Applied variants are exactly those `render_batch` emits:
/// - `PublishToTile.node` → [`SceneGraph::set_tile_root_checked`] (the transcript
///   markdown root — the actual visible content).
/// - `UpdateTileInputMode` → [`SceneGraph::update_tile_input_mode`].
/// - `SetTileLifecycleAccent` → [`SceneGraph::set_tile_lifecycle_accent`] /
///   [`SceneGraph::clear_tile_lifecycle_accent`].
/// - `SetTileUnreadCount` → [`SceneGraph::set_tile_unread_count`] (the ambient
///   jump-to-latest pill badge count; `0` clears it).
/// - `SetTileComposerInteraction` → [`SceneGraph::set_tile_composer_interaction`]
///   / [`SceneGraph::clear_tile_composer_interaction`] (the composer hit region as
///   coalescible overlay state; the scene derives + re-attaches the node under the
///   tile root after each republish, so an interaction-enabled streaming portal
///   stays StateStream-coalescible, hud-iofav). `AddNode` is still accepted here
///   for any other caller, but `render_batch` no longer produces one.
/// - `SetPortalSurface` → [`SceneGraph::set_portal_surface`] (the one-time
///   first-class 8-part surface declaration, hud-rpm9s).
/// - `UpdatePortalSurfaceState` → [`SceneGraph::update_portal_surface_state`]
///   (the per-render coalescible lifecycle/display patch, hud-rpm9s).
///
/// Tile geometry (`PublishToTile.bounds`) is intentionally NOT applied: the
/// in-process driver owns tile placement and scroll geometry (it sizes the tile
/// on create and tracks content/viewport height through
/// `notify_tile_content_appended`). Re-applying the adapter's static config
/// bounds here would fight that path, so bounds are left to the driver.
///
/// The `tile_id` and `namespace` are supplied by the caller and authoritative;
/// any tile-id bytes carried in the proto mutations are ignored. Per-mutation
/// failures are logged and skipped rather than aborting the whole batch,
/// mirroring the session server's per-variant warn-and-skip on malformed input.
pub fn apply_portal_render_batch_to_scene(
    scene: &mut SceneGraph,
    tile_id: SceneId,
    namespace: &str,
    batch: &proto::session::MutationBatch,
) {
    use crate::proto::mutation_proto::Mutation;

    for m in &batch.mutations {
        match &m.mutation {
            Some(Mutation::PublishToTile(pt)) => {
                // Bounds are deliberately skipped (driver owns geometry). Only the
                // content root node is applied — this is the transcript paint.
                let Some(node_proto) = pt.node.as_ref() else {
                    continue;
                };
                // Materialize the whole inline subtree atomically (hud-ga4md): a
                // multi-part portal body (transcript + head-anchored composer +
                // INPUT band) arrives as one `NodeProto` with `children`, and the
                // flat root-first list is set as the tile root in a single
                // mutation — no per-part `AddNode` that would flip this batch
                // Transactional and break republish coalescing (hud-mzk74). A
                // childless node yields a one-element list = today's flat paint.
                match proto_node_tree_to_scene(node_proto) {
                    Some(mut nodes) => {
                        let root = nodes.remove(0);
                        if let Err(e) =
                            scene.set_tile_root_tree_checked(tile_id, root, nodes, namespace)
                        {
                            tracing::warn!(
                                ?e,
                                "portal in-process apply: SetTileRoot failed — content not painted"
                            );
                        }
                    }
                    None => {
                        tracing::warn!(
                            "portal in-process apply: PublishToTile node invalid; content skipped"
                        );
                    }
                }
            }
            Some(Mutation::UpdateTileInputMode(utim)) => {
                let input_mode = proto_input_mode_to_scene(
                    proto::TileInputModeProto::try_from(utim.input_mode)
                        .unwrap_or(proto::TileInputModeProto::TileInputModeUnspecified),
                );
                if let Err(e) = scene.update_tile_input_mode(tile_id, input_mode, namespace) {
                    tracing::warn!(?e, "portal in-process apply: UpdateTileInputMode failed");
                }
            }
            Some(Mutation::SetTileLifecycleAccent(sla)) => {
                // Absent / zero-alpha color or non-positive width = clear (mirrors
                // the session-server SetTileLifecycleAccent conversion).
                let accent = sla.color.as_ref().and_then(|c| {
                    (sla.width_px > 0.0 && c.a > 0.0).then(|| LifecycleAccent {
                        color: proto_rgba_to_scene(c),
                        width_px: sla.width_px,
                    })
                });
                match accent {
                    // Checked variants enforce the same live-lease + `ModifyOwnTiles`
                    // gate as the sibling `set_tile_root_checked` content paint in
                    // this batch (hud-a745w): a suspended/orphaned/expired lease must
                    // not mutate the accent overlay or bump `scene.version`, so the
                    // accent cannot escape safe-mode/lease suspension here (this path
                    // bypasses the `apply_batch` Stage-1 lease check). The lease-grace
                    // degraded repaint reconnects the driver lease to Active before
                    // rendering, so it still applies.
                    Some(accent) => {
                        if let Err(e) =
                            scene.set_tile_lifecycle_accent_checked(tile_id, accent, namespace)
                        {
                            tracing::warn!(
                                ?e,
                                "portal in-process apply: SetTileLifecycleAccent failed"
                            );
                        }
                    }
                    None => {
                        if let Err(e) =
                            scene.clear_tile_lifecycle_accent_checked(tile_id, namespace)
                        {
                            tracing::warn!(
                                ?e,
                                "portal in-process apply: ClearTileLifecycleAccent failed"
                            );
                        }
                    }
                }
            }
            Some(Mutation::SetTileUnreadCount(stuc)) => {
                // Ambient unread-output count for the jump-to-latest pill badge
                // (hud-hwk2m). `0` clears the badge. The in-process driver also
                // sets this directly at its drain site with the identical
                // `unread_output_count.unwrap_or(0)` value, so applying the
                // co-travelling mutation here is idempotent — but keeping the arm
                // means the render batch stays fully self-describing, and it is
                // exactly this mutation a bridged portal relies on over the wire.
                // Checked path (matching the SetTileLifecycleAccent arm above): a
                // suspended/orphaned/expired or `ModifyOwnTiles`-revoked lease must
                // not mutate the overlay or bump `scene.version` (hud-a745w).
                if let Err(e) =
                    scene.set_tile_unread_count_checked(tile_id, stuc.count as usize, namespace)
                {
                    tracing::warn!(?e, "portal in-process apply: SetTileUnreadCount failed");
                }
            }
            Some(Mutation::SetTileComposerInteraction(stci)) => {
                // Composer interaction hit region (hud-iofav). `composer = None`
                // clears it (interaction disabled). The runtime stores the spec as
                // overlay state and derives/re-attaches the hit-region scene node
                // after each transcript republish — so it never rides a
                // per-republish `AddNode` (which would flip the batch Transactional,
                // hud-mzk74). Checked path (matching the sibling accent/unread arms):
                // a suspended/orphaned/expired or `ModifyOwnTiles`-revoked lease must
                // not mutate the overlay or bump `scene.version` (hud-a745w).
                match stci.composer.as_ref() {
                    Some(hr) => {
                        let region = proto_hit_region_to_scene(hr);
                        if let Err(e) =
                            scene.set_tile_composer_interaction_checked(tile_id, region, namespace)
                        {
                            tracing::warn!(
                                ?e,
                                "portal in-process apply: SetTileComposerInteraction failed"
                            );
                        }
                    }
                    None => {
                        if let Err(e) =
                            scene.clear_tile_composer_interaction_checked(tile_id, namespace)
                        {
                            tracing::warn!(
                                ?e,
                                "portal in-process apply: ClearTileComposerInteraction failed"
                            );
                        }
                    }
                }
            }
            Some(Mutation::AddNode(an)) => {
                // parent_id is big-endian RFC 4122 bytes (render_batch uses the
                // root node's `as_bytes()`); the root node's own id was encoded
                // little-endian and decoded as such by `proto_node_to_scene`, so
                // both resolve to the same UUID and the composer node parents the
                // markdown root. An empty parent_id means "attach to tile root".
                let parent_id = if an.parent_id.is_empty() {
                    None
                } else {
                    match scene_id_from_be_bytes(&an.parent_id) {
                        Some(id) => Some(id),
                        None => {
                            tracing::warn!(
                                parent_id_len = an.parent_id.len(),
                                "portal in-process apply: AddNode invalid parent_id; skipped"
                            );
                            continue;
                        }
                    }
                };
                let Some(node_proto) = an.node.as_ref() else {
                    continue;
                };
                match proto_node_to_scene(node_proto) {
                    Some(node) => {
                        if let Err(e) =
                            scene.add_node_to_tile_checked(tile_id, parent_id, node, namespace)
                        {
                            tracing::warn!(?e, "portal in-process apply: AddNode failed");
                        }
                    }
                    None => {
                        tracing::warn!("portal in-process apply: AddNode node invalid; skipped");
                    }
                }
            }
            // ── First-class portal surface (RFC 0013 §7.2 promotion; hud-rpm9s) ──
            //
            // The cooperative adapter drives the promoted portal through the
            // first-class surface API in addition to the raw-tile assembly above:
            // a one-time `SetPortalSurface` declares the governed 8-part descriptor
            // (Transactional, structural) and a per-render `UpdatePortalSurfaceState`
            // patches lifecycle/display (coalescible StateStream). Applied here so
            // the in-process driver path reaches parity with the wire path, which
            // already decodes these variants (session_server/mutations.rs).
            Some(Mutation::SetPortalSurface(sps)) => {
                let Some(surface_proto) = sps.surface.as_ref() else {
                    tracing::warn!(
                        "portal in-process apply: SetPortalSurface missing surface; skipped"
                    );
                    continue;
                };
                match proto_portal_surface_to_scene(surface_proto) {
                    Ok(surface) => {
                        if let Err(e) = scene.set_portal_surface(tile_id, surface, namespace) {
                            tracing::warn!(
                                ?e,
                                "portal in-process apply: SetPortalSurface failed — \
                                 surface not declared"
                            );
                        }
                    }
                    Err(reason) => {
                        tracing::warn!(
                            %reason,
                            "portal in-process apply: SetPortalSurface invalid surface; skipped"
                        );
                    }
                }
            }
            Some(Mutation::UpdatePortalSurfaceState(ups)) => {
                // Coalescible patch: UNSPECIFIED wire values decode to `None`
                // (leave-unchanged). A missing surface (patch before the one-time
                // declaration) is benign — logged and skipped, mirroring the
                // wire path's per-variant warn-and-skip.
                let lifecycle = proto_portal_lifecycle_to_scene(ups.lifecycle);
                let display_state = proto_portal_display_state_to_scene(ups.display_state);
                if let Err(e) =
                    scene.update_portal_surface_state(tile_id, lifecycle, display_state, namespace)
                {
                    tracing::warn!(
                        ?e,
                        "portal in-process apply: UpdatePortalSurfaceState skipped"
                    );
                }
            }
            // `ResidentGrpcPortalAdapter::render_batch{,_with_surface}` is the SOLE
            // producer of the batches reaching here (raw-tile content, the
            // lifecycle accent, unread-count, and the first-class surface
            // mutations above). Any other variant is silently ignored: if it ever
            // grows a new variant, add an explicit arm here (and a paint assertion
            // in `drain_paints_published_transcript_onto_tile`) so it is not
            // dropped.
            _ => {}
        }
    }
}

/// Decode a 16-byte big-endian (RFC 4122) UUID into a [`SceneId`].
///
/// Returns `None` if `bytes` is not exactly 16 bytes. Mirrors the session
/// server's `bytes_to_scene_id` (which decodes `parent_id` the same way) so the
/// in-process apply path resolves AddNode parents identically to the wire path.
fn scene_id_from_be_bytes(bytes: &[u8]) -> Option<SceneId> {
    let arr: [u8; 16] = bytes.try_into().ok()?;
    Some(SceneId::from_uuid(uuid::Uuid::from_bytes(arr)))
}

// ─── Portal surface conversions (RFC 0013 §7.2 promotion; hud-tc153) ──────────

/// Decode a `PortalPeerClassProto` i32 to the scene [`PortalPeerClass`].
/// Unknown/unspecified wire values map to [`PortalPeerClass::Unspecified`].
pub fn proto_portal_peer_class_to_scene(v: i32) -> PortalPeerClass {
    match proto::PortalPeerClassProto::try_from(v) {
        Ok(proto::PortalPeerClassProto::PortalPeerClassResidentLlm) => PortalPeerClass::ResidentLlm,
        Ok(proto::PortalPeerClassProto::PortalPeerClassOperator) => PortalPeerClass::Operator,
        Ok(proto::PortalPeerClassProto::PortalPeerClassHumanPeer) => PortalPeerClass::HumanPeer,
        Ok(proto::PortalPeerClassProto::PortalPeerClassAdapter) => PortalPeerClass::Adapter,
        _ => PortalPeerClass::Unspecified,
    }
}

/// Encode a scene [`PortalPeerClass`] to its `PortalPeerClassProto` i32.
pub fn scene_portal_peer_class_to_proto(p: PortalPeerClass) -> i32 {
    let e = match p {
        PortalPeerClass::Unspecified => proto::PortalPeerClassProto::PortalPeerClassUnspecified,
        PortalPeerClass::ResidentLlm => proto::PortalPeerClassProto::PortalPeerClassResidentLlm,
        PortalPeerClass::Operator => proto::PortalPeerClassProto::PortalPeerClassOperator,
        PortalPeerClass::HumanPeer => proto::PortalPeerClassProto::PortalPeerClassHumanPeer,
        PortalPeerClass::Adapter => proto::PortalPeerClassProto::PortalPeerClassAdapter,
    };
    e as i32
}

/// Decode a `PortalLifecycleStateProto` i32 to the scene enum (total; unknown /
/// unspecified map to [`PortalLifecycleState::Unspecified`]).
pub fn proto_portal_lifecycle_enum_to_scene(v: i32) -> PortalLifecycleState {
    match proto::PortalLifecycleStateProto::try_from(v) {
        Ok(proto::PortalLifecycleStateProto::PortalLifecycleStateActive) => {
            PortalLifecycleState::Active
        }
        Ok(proto::PortalLifecycleStateProto::PortalLifecycleStateWaitingForInput) => {
            PortalLifecycleState::WaitingForInput
        }
        Ok(proto::PortalLifecycleStateProto::PortalLifecycleStateBlocked) => {
            PortalLifecycleState::Blocked
        }
        Ok(proto::PortalLifecycleStateProto::PortalLifecycleStateDegraded) => {
            PortalLifecycleState::Degraded
        }
        Ok(proto::PortalLifecycleStateProto::PortalLifecycleStateDetached) => {
            PortalLifecycleState::Detached
        }
        _ => PortalLifecycleState::Unspecified,
    }
}

/// Decode a `PortalLifecycleStateProto` i32 as a coalescible **patch**: an
/// UNSPECIFIED wire value means "leave unchanged" and maps to `None`.
pub fn proto_portal_lifecycle_to_scene(v: i32) -> Option<PortalLifecycleState> {
    match proto_portal_lifecycle_enum_to_scene(v) {
        PortalLifecycleState::Unspecified => None,
        other => Some(other),
    }
}

/// Encode a scene [`PortalLifecycleState`] to its proto i32.
pub fn scene_portal_lifecycle_to_proto(s: PortalLifecycleState) -> i32 {
    let e = match s {
        PortalLifecycleState::Unspecified => {
            proto::PortalLifecycleStateProto::PortalLifecycleStateUnspecified
        }
        PortalLifecycleState::Active => {
            proto::PortalLifecycleStateProto::PortalLifecycleStateActive
        }
        PortalLifecycleState::WaitingForInput => {
            proto::PortalLifecycleStateProto::PortalLifecycleStateWaitingForInput
        }
        PortalLifecycleState::Blocked => {
            proto::PortalLifecycleStateProto::PortalLifecycleStateBlocked
        }
        PortalLifecycleState::Degraded => {
            proto::PortalLifecycleStateProto::PortalLifecycleStateDegraded
        }
        PortalLifecycleState::Detached => {
            proto::PortalLifecycleStateProto::PortalLifecycleStateDetached
        }
    };
    e as i32
}

/// Decode a `PortalDisplayStateProto` i32 to the scene enum (total; unknown /
/// unspecified map to [`PortalDisplayState::Unspecified`]).
pub fn proto_portal_display_state_enum_to_scene(v: i32) -> PortalDisplayState {
    match proto::PortalDisplayStateProto::try_from(v) {
        Ok(proto::PortalDisplayStateProto::PortalDisplayStateCollapsed) => {
            PortalDisplayState::Collapsed
        }
        Ok(proto::PortalDisplayStateProto::PortalDisplayStateExpanded) => {
            PortalDisplayState::Expanded
        }
        _ => PortalDisplayState::Unspecified,
    }
}

/// Decode a `PortalDisplayStateProto` i32 as a coalescible **patch**: an
/// UNSPECIFIED wire value means "leave unchanged" and maps to `None`.
pub fn proto_portal_display_state_to_scene(v: i32) -> Option<PortalDisplayState> {
    match proto_portal_display_state_enum_to_scene(v) {
        PortalDisplayState::Unspecified => None,
        other => Some(other),
    }
}

/// Encode a scene [`PortalDisplayState`] to its proto i32.
pub fn scene_portal_display_state_to_proto(s: PortalDisplayState) -> i32 {
    let e = match s {
        PortalDisplayState::Unspecified => {
            proto::PortalDisplayStateProto::PortalDisplayStateUnspecified
        }
        PortalDisplayState::Collapsed => {
            proto::PortalDisplayStateProto::PortalDisplayStateCollapsed
        }
        PortalDisplayState::Expanded => proto::PortalDisplayStateProto::PortalDisplayStateExpanded,
    };
    e as i32
}

/// Decode a `PortalPartKindProto` i32 to the scene [`PortalPartKind`].
/// UNSPECIFIED / unknown values return `None` (a part with no valid kind is
/// dropped by the caller).
pub fn proto_portal_part_kind_to_scene(v: i32) -> Option<PortalPartKind> {
    match proto::PortalPartKindProto::try_from(v) {
        Ok(proto::PortalPartKindProto::PortalPartKindFrame) => Some(PortalPartKind::Frame),
        Ok(proto::PortalPartKindProto::PortalPartKindHeader) => Some(PortalPartKind::Header),
        Ok(proto::PortalPartKindProto::PortalPartKindComposer) => Some(PortalPartKind::Composer),
        Ok(proto::PortalPartKindProto::PortalPartKindTranscript) => {
            Some(PortalPartKind::Transcript)
        }
        Ok(proto::PortalPartKindProto::PortalPartKindDivider) => Some(PortalPartKind::Divider),
        Ok(proto::PortalPartKindProto::PortalPartKindCollapsedCard) => {
            Some(PortalPartKind::CollapsedCard)
        }
        Ok(proto::PortalPartKindProto::PortalPartKindCaptureBackstop) => {
            Some(PortalPartKind::CaptureBackstop)
        }
        Ok(proto::PortalPartKindProto::PortalPartKindGestureShield) => {
            Some(PortalPartKind::GestureShield)
        }
        _ => None,
    }
}

/// Encode a scene [`PortalPartKind`] to its `PortalPartKindProto` i32.
pub fn scene_portal_part_kind_to_proto(k: PortalPartKind) -> i32 {
    let e = match k {
        PortalPartKind::Frame => proto::PortalPartKindProto::PortalPartKindFrame,
        PortalPartKind::Header => proto::PortalPartKindProto::PortalPartKindHeader,
        PortalPartKind::Composer => proto::PortalPartKindProto::PortalPartKindComposer,
        PortalPartKind::Transcript => proto::PortalPartKindProto::PortalPartKindTranscript,
        PortalPartKind::Divider => proto::PortalPartKindProto::PortalPartKindDivider,
        PortalPartKind::CollapsedCard => proto::PortalPartKindProto::PortalPartKindCollapsedCard,
        PortalPartKind::CaptureBackstop => {
            proto::PortalPartKindProto::PortalPartKindCaptureBackstop
        }
        PortalPartKind::GestureShield => proto::PortalPartKindProto::PortalPartKindGestureShield,
    };
    e as i32
}

/// Convert a `PortalSurfaceProto` to a scene [`PortalSurface`].
///
/// Node references (`PortalPartProto.node`) are decoded as 16-byte big-endian
/// UUIDs, matching the session server's node/tile addressing (`bytes_to_scene_id`)
/// so a part resolves against the tile's node tree the same way `UpdateNodeContent`
/// does. A part whose `kind` is UNSPECIFIED/unknown, or whose non-empty `node`
/// is not 16 bytes, is rejected. The resulting surface must pass
/// [`PortalSurface::validate_structure`].
pub fn proto_portal_surface_to_scene(
    p: &proto::PortalSurfaceProto,
) -> Result<PortalSurface, String> {
    let identity = p
        .identity
        .as_ref()
        .map(|i| PortalIdentity {
            session_id: i.session_id.clone(),
            display_name: i.display_name.clone(),
            peer_class: proto_portal_peer_class_to_scene(i.peer_class),
        })
        .unwrap_or_default();

    let mut parts = Vec::with_capacity(p.parts.len());
    for part in &p.parts {
        let Some(kind) = proto_portal_part_kind_to_scene(part.kind) else {
            return Err(format!(
                "part has unspecified/unknown kind (wire value {})",
                part.kind
            ));
        };
        let bounds = part
            .bounds
            .as_ref()
            .map(proto_rect_to_scene)
            .unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0));
        let node = if part.node.is_empty() {
            None
        } else {
            Some(
                scene_id_from_be_bytes(&part.node)
                    .ok_or_else(|| format!("part {kind:?} has invalid node id length"))?,
            )
        };
        parts.push(PortalPart { kind, bounds, node });
    }

    let surface = PortalSurface {
        identity,
        lifecycle: proto_portal_lifecycle_enum_to_scene(p.lifecycle),
        display_state: proto_portal_display_state_enum_to_scene(p.display_state),
        parts,
    };
    surface.validate_structure()?;
    Ok(surface)
}

/// Convert a scene [`PortalSurface`] to its `PortalSurfaceProto` wire form.
pub fn scene_portal_surface_to_proto(s: &PortalSurface) -> proto::PortalSurfaceProto {
    proto::PortalSurfaceProto {
        identity: Some(proto::PortalIdentityProto {
            session_id: s.identity.session_id.clone(),
            display_name: s.identity.display_name.clone(),
            peer_class: scene_portal_peer_class_to_proto(s.identity.peer_class),
        }),
        lifecycle: scene_portal_lifecycle_to_proto(s.lifecycle),
        display_state: scene_portal_display_state_to_proto(s.display_state),
        parts: s
            .parts
            .iter()
            .map(|part| proto::PortalPartProto {
                kind: scene_portal_part_kind_to_proto(part.kind),
                bounds: Some(proto::Rect {
                    x: part.bounds.x,
                    y: part.bounds.y,
                    width: part.bounds.width,
                    height: part.bounds.height,
                }),
                node: part
                    .node
                    .map(|id| id.as_uuid().as_bytes().to_vec())
                    .unwrap_or_default(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Portal surface proto round-trips (RFC 0013 §7.2; hud-tc153) ──────────

    #[test]
    fn portal_surface_scene_proto_round_trip_preserves_all_fields() {
        let node_id = SceneId::new();
        let surface = PortalSurface {
            identity: PortalIdentity {
                session_id: "sess-42".to_string(),
                display_name: "Operator".to_string(),
                peer_class: PortalPeerClass::Operator,
            },
            lifecycle: PortalLifecycleState::WaitingForInput,
            display_state: PortalDisplayState::Collapsed,
            parts: vec![
                PortalPart {
                    kind: PortalPartKind::Transcript,
                    bounds: Rect::new(1.0, 2.0, 300.0, 200.0),
                    node: Some(node_id),
                },
                PortalPart {
                    kind: PortalPartKind::Divider,
                    bounds: Rect::new(150.0, 0.0, 2.0, 200.0),
                    node: None,
                },
            ],
        };

        let proto = scene_portal_surface_to_proto(&surface);
        let restored =
            proto_portal_surface_to_scene(&proto).expect("valid surface must round-trip");
        assert_eq!(restored, surface);
        // The materialized part node id survives the big-endian byte round-trip.
        assert_eq!(restored.parts[0].node, Some(node_id));
    }

    #[test]
    fn portal_state_patch_unspecified_means_unchanged() {
        // UNSPECIFIED wire values decode to None ("leave unchanged") for the
        // coalescible patch path.
        assert_eq!(
            proto_portal_lifecycle_to_scene(
                proto::PortalLifecycleStateProto::PortalLifecycleStateUnspecified as i32
            ),
            None
        );
        assert_eq!(
            proto_portal_display_state_to_scene(
                proto::PortalDisplayStateProto::PortalDisplayStateUnspecified as i32
            ),
            None
        );
        // A concrete value decodes to Some.
        assert_eq!(
            proto_portal_lifecycle_to_scene(
                proto::PortalLifecycleStateProto::PortalLifecycleStateBlocked as i32
            ),
            Some(PortalLifecycleState::Blocked)
        );
    }

    #[test]
    fn portal_surface_conversion_rejects_unspecified_part_kind() {
        let proto = proto::PortalSurfaceProto {
            identity: None,
            lifecycle: 0,
            display_state: 0,
            parts: vec![proto::PortalPartProto {
                kind: proto::PortalPartKindProto::PortalPartKindUnspecified as i32,
                bounds: None,
                node: vec![],
            }],
        };
        assert!(proto_portal_surface_to_scene(&proto).is_err());
    }

    #[test]
    fn portal_surface_conversion_rejects_bad_node_length() {
        let proto = proto::PortalSurfaceProto {
            identity: None,
            lifecycle: 0,
            display_state: 0,
            parts: vec![proto::PortalPartProto {
                kind: proto::PortalPartKindProto::PortalPartKindTranscript as i32,
                bounds: None,
                node: vec![1, 2, 3], // not 16 bytes
            }],
        };
        assert!(proto_portal_surface_to_scene(&proto).is_err());
    }

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

    // ── HitRegionNode accepts_composer_input proto round-trip (hud-hxe91) ────

    /// Proto→scene conversion via proto_node_to_scene must carry
    /// accepts_composer_input=true from wire to scene graph.
    #[test]
    fn hit_region_proto_to_scene_carries_accepts_composer_input() {
        let proto_node = crate::proto::NodeProto {
            layout: 0,
            id: Vec::new(),
            data: Some(crate::proto::node_proto::Data::HitRegion(
                crate::proto::HitRegionNodeProto {
                    bounds: Some(crate::proto::Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 400.0,
                        height: 100.0,
                    }),
                    interaction_id: "composer-region".to_string(),
                    accepts_focus: true,
                    accepts_pointer: false,
                    auto_capture: false,
                    release_on_up: false,
                    accepts_composer_input: true,
                },
            )),
            children: vec![],
        };
        let scene_node = proto_node_to_scene(&proto_node)
            .expect("valid HitRegionNodeProto must convert to scene Node");
        match &scene_node.data {
            NodeData::HitRegion(hr) => {
                assert!(
                    hr.accepts_composer_input,
                    "accepts_composer_input must be true after proto→scene conversion"
                );
                assert_eq!(hr.interaction_id, "composer-region");
                assert!(hr.accepts_focus);
                assert!(!hr.accepts_pointer);
            }
            other => panic!("expected HitRegion node, got {other:?}"),
        }
    }

    /// Proto→scene conversion via proto_update_node_content_data_to_scene must
    /// also carry accepts_composer_input=true (UpdateNodeContent path).
    #[test]
    fn hit_region_update_node_content_path_carries_accepts_composer_input() {
        let data = crate::proto::update_node_content_mutation::Data::HitRegion(
            crate::proto::HitRegionNodeProto {
                bounds: Some(crate::proto::Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 200.0,
                    height: 50.0,
                }),
                interaction_id: "update-composer-region".to_string(),
                accepts_focus: true,
                accepts_pointer: true,
                auto_capture: false,
                release_on_up: true,
                accepts_composer_input: true,
            },
        );
        let node_data = proto_update_node_content_data_to_scene(&data)
            .expect("valid HitRegionNodeProto must convert to NodeData");
        match node_data {
            NodeData::HitRegion(hr) => {
                assert!(
                    hr.accepts_composer_input,
                    "accepts_composer_input must be true after UpdateNodeContent proto→scene"
                );
                assert!(hr.release_on_up);
            }
            other => panic!("expected HitRegion NodeData, got {other:?}"),
        }
    }

    /// Scene→proto round-trip: accepts_composer_input survives both directions.
    #[test]
    fn hit_region_accepts_composer_input_round_trips_scene_to_proto() {
        let scene_node = Node {
            layout: Default::default(),
            id: SceneId::new(),
            children: vec![],
            data: NodeData::HitRegion(HitRegionNode {
                bounds: Rect::new(0.0, 0.0, 300.0, 60.0),
                interaction_id: "roundtrip-composer".to_string(),
                accepts_focus: true,
                accepts_pointer: false,
                accepts_composer_input: true,
                ..Default::default()
            }),
        };
        let proto_node = scene_node_to_proto(&scene_node);
        match proto_node.data {
            Some(crate::proto::node_proto::Data::HitRegion(hr)) => {
                assert!(
                    hr.accepts_composer_input,
                    "accepts_composer_input must survive scene→proto conversion"
                );
                assert_eq!(hr.interaction_id, "roundtrip-composer");
            }
            other => panic!("expected HitRegion proto data, got {other:?}"),
        }
    }

    /// Default (false) is preserved: proto with accepts_composer_input unset
    /// must decode to accepts_composer_input=false (proto3 default).
    #[test]
    fn hit_region_proto_accepts_composer_input_defaults_false() {
        let proto_node = crate::proto::NodeProto {
            layout: 0,
            id: Vec::new(),
            data: Some(crate::proto::node_proto::Data::HitRegion(
                crate::proto::HitRegionNodeProto {
                    bounds: None,
                    interaction_id: "no-composer".to_string(),
                    accepts_focus: false,
                    accepts_pointer: true,
                    auto_capture: false,
                    release_on_up: false,
                    accepts_composer_input: false,
                },
            )),
            children: vec![],
        };
        let scene_node = proto_node_to_scene(&proto_node).unwrap();
        match &scene_node.data {
            NodeData::HitRegion(hr) => {
                assert!(
                    !hr.accepts_composer_input,
                    "accepts_composer_input must be false when not set in proto"
                );
            }
            other => panic!("expected HitRegion, got {other:?}"),
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
