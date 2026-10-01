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

/// Convert a scene ZoneContent to a protobuf ZoneContent.
pub fn scene_zone_content_to_proto(c: &ZoneContent) -> proto::ZoneContent {
    use proto::zone_content::Payload;
    let payload = match c {
        ZoneContent::StreamText(s) => Payload::StreamText(s.clone()),
        ZoneContent::Notification(n) => Payload::Notification(proto::NotificationPayload {
            text: n.text.clone(),
            icon: n.icon.clone(),
            urgency: n.urgency,
            title: n.title.clone(),
            actions: n
                .actions
                .iter()
                .map(|a| proto::NotificationActionProto {
                    label: a.label.clone(),
                    callback_id: a.callback_id.clone(),
                })
                .collect(),
        }),
        ZoneContent::StatusBar(sb) => Payload::StatusBar(proto::StatusBarPayload {
            entries: sb.entries.clone(),
        }),
        ZoneContent::SolidColor(c) => Payload::SolidColor(proto::Rgba {
            r: c.r,
            g: c.g,
            b: c.b,
            a: c.a,
        }),
        // StaticImage → StaticImageRef: encode the 32-byte BLAKE3 hash (WM-S2b types.proto delta).
        ZoneContent::StaticImage(resource_id) => Payload::StaticImageRef(proto::StaticImageRef {
            resource_id: resource_id.as_bytes().to_vec(),
        }),
    };
    proto::ZoneContent {
        payload: Some(payload),
    }
}

/// Convert a scene ZoneRegistrySnapshot to a protobuf ZoneRegistrySnapshotProto.
pub fn zone_registry_snapshot_to_proto(
    snap: &ZoneRegistrySnapshot,
) -> proto::ZoneRegistrySnapshotProto {
    let zones = snap.zones.iter().map(zone_definition_to_proto).collect();
    let active_publishes = snap
        .active_publishes
        .iter()
        .map(zone_publish_record_to_proto)
        .collect();
    proto::ZoneRegistrySnapshotProto {
        zones,
        active_publishes,
    }
}

/// Convert a scene ZoneDefinition to a protobuf ZoneDefinitionProto.
pub fn zone_definition_to_proto(z: &ZoneDefinition) -> proto::ZoneDefinitionProto {
    let geometry_policy = Some(geometry_policy_to_proto(&z.geometry_policy));
    let accepted_media_types = z
        .accepted_media_types
        .iter()
        .map(|mt| format!("{mt:?}"))
        .collect();
    let rendering_policy = Some(rendering_policy_to_proto(&z.rendering_policy));

    let (contention, stack_max_depth, merge_max_keys) = match z.contention_policy {
        ContentionPolicy::LatestWins => (
            proto::ContentionPolicyProto::ContentionPolicyLatestWins as i32,
            0,
            0,
        ),
        ContentionPolicy::Replace => (
            proto::ContentionPolicyProto::ContentionPolicyReplace as i32,
            0,
            0,
        ),
        ContentionPolicy::Stack { max_depth } => (
            proto::ContentionPolicyProto::ContentionPolicyStack as i32,
            max_depth as u32,
            0,
        ),
        ContentionPolicy::MergeByKey { max_keys } => (
            proto::ContentionPolicyProto::ContentionPolicyMergeByKey as i32,
            0,
            max_keys as u32,
        ),
    };

    proto::ZoneDefinitionProto {
        id: z.id.to_string(),
        name: z.name.clone(),
        description: z.description.clone(),
        geometry_policy,
        accepted_media_types,
        rendering_policy,
        contention_policy: contention,
        max_publishers: z.max_publishers,
        auto_clear_ms: z.auto_clear_ms.unwrap_or(0),
        stack_max_depth,
        merge_max_keys,
        ephemeral: z.ephemeral,
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

/// Convert a scene RenderingPolicy to a protobuf RenderingPolicyProto.
pub fn rendering_policy_to_proto(rp: &RenderingPolicy) -> proto::RenderingPolicyProto {
    proto::RenderingPolicyProto {
        // Fields 1-4 (original; must not be renumbered)
        font_size_px: rp.font_size_px.unwrap_or(0.0),
        backdrop: rp.backdrop.map(|c| proto::Rgba {
            r: c.r,
            g: c.g,
            b: c.b,
            a: c.a,
        }),
        text_align: rp
            .text_align
            .map(|ta| match ta {
                TextAlign::Start => proto::TextAlignProto::Start as i32,
                TextAlign::Center => proto::TextAlignProto::Center as i32,
                TextAlign::End => proto::TextAlignProto::End as i32,
            })
            .unwrap_or(proto::TextAlignProto::Unspecified as i32),
        margin_px: rp.margin_px.unwrap_or(0.0),
        // Fields 5-14 (extended policy fields)
        font_family: rp
            .font_family
            .map(|ff| match ff {
                FontFamily::SystemSansSerif => proto::FontFamilyProto::SystemSans as i32,
                FontFamily::SystemMonospace => proto::FontFamilyProto::SystemMonospace as i32,
                FontFamily::SystemSerif => proto::FontFamilyProto::SystemSerif as i32,
            })
            .unwrap_or(proto::FontFamilyProto::Unspecified as i32),
        font_weight: rp.font_weight.unwrap_or(0) as u32,
        text_color: rp.text_color.map(|c| proto::Rgba {
            r: c.r,
            g: c.g,
            b: c.b,
            a: c.a,
        }),
        // Sentinel: -1.0 = not set (proto float default 0.0 is valid for this field)
        backdrop_opacity: rp.backdrop_opacity.unwrap_or(-1.0),
        outline_color: rp.outline_color.map(|c| proto::Rgba {
            r: c.r,
            g: c.g,
            b: c.b,
            a: c.a,
        }),
        outline_width: rp.outline_width.unwrap_or(0.0),
        // Sentinel: -1.0 = not set (0.0 is a valid margin)
        margin_horizontal: rp.margin_horizontal.unwrap_or(-1.0),
        margin_vertical: rp.margin_vertical.unwrap_or(-1.0),
        transition_in_ms: rp.transition_in_ms.unwrap_or(0),
        transition_out_ms: rp.transition_out_ms.unwrap_or(0),
        overflow: rp
            .overflow
            .map(|ov| match ov {
                TextOverflow::Clip => proto::TextOverflowProto::Clip as i32,
                TextOverflow::Ellipsis => proto::TextOverflowProto::Ellipsis as i32,
            })
            .unwrap_or(proto::TextOverflowProto::Unspecified as i32),
        // Sentinel: -1.0 = not set (0.0 is a valid radius)
        backdrop_radius: rp.backdrop_radius.unwrap_or(-1.0),
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

/// Convert a scene ZonePublishRecord to a protobuf ZonePublishRecordProto.
///
/// Snapshot parity fields (WM-S2b §4, RFC 0014 §3.6): `expires_at_wall_us`,
/// `content_classification`, and `breakpoints` are included so that
/// `SceneSnapshot.snapshot_json` carries complete declarative publication state
/// for reconnect reconciliation without transport internals.
pub fn zone_publish_record_to_proto(r: &ZonePublishRecord) -> proto::ZonePublishRecordProto {
    proto::ZonePublishRecordProto {
        zone_name: r.zone_name.clone(),
        publisher_namespace: r.publisher_namespace.clone(),
        content: Some(scene_zone_content_to_proto(&r.content)),
        published_at_wall_us: r.published_at_wall_us,
        merge_key: r.merge_key.clone().unwrap_or_default(),
        // Snapshot parity fields (WM-S2b types.proto delta §4).
        expires_at_wall_us: r.expires_at_wall_us.unwrap_or(0),
        content_classification: r.content_classification.clone().unwrap_or_default(),
        breakpoints: r.breakpoints.clone(),
    }
}

/// Convert a protobuf GeometryPolicyProto to scene GeometryPolicy.
///
/// Returns `None` if the policy oneof is absent (empty proto).
pub fn proto_to_geometry_policy(p: &proto::GeometryPolicyProto) -> Option<GeometryPolicy> {
    use proto::geometry_policy_proto::Policy;
    match p.policy.as_ref()? {
        Policy::Relative(r) => Some(GeometryPolicy::Relative {
            x_pct: r.x_pct,
            y_pct: r.y_pct,
            width_pct: r.width_pct,
            height_pct: r.height_pct,
        }),
        Policy::EdgeAnchored(e) => {
            let edge = match proto::DisplayEdge::try_from(e.edge)
                .unwrap_or(proto::DisplayEdge::Unspecified)
            {
                proto::DisplayEdge::Top | proto::DisplayEdge::Unspecified => DisplayEdge::Top,
                proto::DisplayEdge::Bottom => DisplayEdge::Bottom,
                proto::DisplayEdge::Left => DisplayEdge::Left,
                proto::DisplayEdge::Right => DisplayEdge::Right,
            };
            Some(GeometryPolicy::EdgeAnchored {
                edge,
                height_pct: e.height_pct,
                width_pct: e.width_pct,
                margin_px: e.margin_px,
            })
        }
    }
}

/// Convert a protobuf RenderingPolicyProto to scene RenderingPolicy.
pub fn proto_to_rendering_policy(p: &proto::RenderingPolicyProto) -> RenderingPolicy {
    // Fields 1-4 (original)
    let font_size_px = if p.font_size_px > 0.0 {
        Some(p.font_size_px)
    } else {
        None
    };
    let backdrop = p.backdrop.as_ref().map(proto_rgba_to_scene);
    let text_align = match proto::TextAlignProto::try_from(p.text_align)
        .unwrap_or(proto::TextAlignProto::Unspecified)
    {
        proto::TextAlignProto::Unspecified => None,
        proto::TextAlignProto::Start => Some(TextAlign::Start),
        proto::TextAlignProto::Center => Some(TextAlign::Center),
        proto::TextAlignProto::End => Some(TextAlign::End),
    };
    let margin_px = if p.margin_px > 0.0 {
        Some(p.margin_px)
    } else {
        None
    };

    // Fields 5-14 (extended policy fields)
    let font_family = match proto::FontFamilyProto::try_from(p.font_family)
        .unwrap_or(proto::FontFamilyProto::Unspecified)
    {
        proto::FontFamilyProto::Unspecified => None,
        proto::FontFamilyProto::SystemSans => Some(FontFamily::SystemSansSerif),
        proto::FontFamilyProto::SystemMonospace => Some(FontFamily::SystemMonospace),
        proto::FontFamilyProto::SystemSerif => Some(FontFamily::SystemSerif),
    };
    // font_weight: 0 = not set; valid range 100-900. Use checked conversion to
    // safely reject malformed values that exceed u16::MAX (e.g. from adversarial proto).
    let font_weight = u16::try_from(p.font_weight).ok().filter(|&w| w > 0);
    // text_color: zero alpha = not set
    let text_color = p
        .text_color
        .as_ref()
        .filter(|c| c.a > 0.0)
        .map(proto_rgba_to_scene);
    // backdrop_opacity: sentinel -1.0 = not set
    let backdrop_opacity = if p.backdrop_opacity >= 0.0 {
        Some(p.backdrop_opacity)
    } else {
        None
    };
    // outline_color: zero alpha = not set
    let outline_color = p
        .outline_color
        .as_ref()
        .filter(|c| c.a > 0.0)
        .map(proto_rgba_to_scene);
    let outline_width = if p.outline_width > 0.0 {
        Some(p.outline_width)
    } else {
        None
    };
    // margin_horizontal / margin_vertical: sentinel -1.0 = not set
    let margin_horizontal = if p.margin_horizontal >= 0.0 {
        Some(p.margin_horizontal)
    } else {
        None
    };
    let margin_vertical = if p.margin_vertical >= 0.0 {
        Some(p.margin_vertical)
    } else {
        None
    };
    let transition_in_ms = if p.transition_in_ms > 0 {
        Some(p.transition_in_ms)
    } else {
        None
    };
    let transition_out_ms = if p.transition_out_ms > 0 {
        Some(p.transition_out_ms)
    } else {
        None
    };
    let overflow = match proto::TextOverflowProto::try_from(p.overflow)
        .unwrap_or(proto::TextOverflowProto::Unspecified)
    {
        proto::TextOverflowProto::Unspecified => None,
        proto::TextOverflowProto::Clip => Some(TextOverflow::Clip),
        proto::TextOverflowProto::Ellipsis => Some(TextOverflow::Ellipsis),
    };

    RenderingPolicy {
        font_size_px,
        backdrop,
        text_align,
        margin_px,
        font_family,
        font_weight,
        text_color,
        backdrop_opacity,
        outline_color,
        outline_width,
        margin_horizontal,
        margin_vertical,
        transition_in_ms,
        transition_out_ms,
        overflow,
        // key_icon_map is intentionally config-layer only. It is NOT
        // transmitted via proto
        // (protocol messages carry payloads, not zone configuration). Any
        // RenderingPolicy reconstructed from proto will have an empty map; the
        // compositor must re-apply zone config after proto-roundtrip if needed.
        key_icon_map: Default::default(),
        // backdrop_radius: sentinel -1.0 = not set (0.0 is a valid radius)
        backdrop_radius: if p.backdrop_radius >= 0.0 {
            Some(p.backdrop_radius)
        } else {
            None
        },
        // stream_tail_anchored is config-layer only (zone configuration / design
        // token). It is NOT transmitted via proto — it is resolved at profile
        // load time and is not a per-publish payload field.
        stream_tail_anchored: None,
    }
}

// ─── Widget conversions ───────────────────────────────────────────────────────

/// Convert a scene WidgetParamType to a proto enum i32.
pub fn widget_param_type_to_proto(t: WidgetParamType) -> i32 {
    match t {
        WidgetParamType::F32 => proto::WidgetParamTypeProto::WidgetParamTypeF32 as i32,
        WidgetParamType::String => proto::WidgetParamTypeProto::WidgetParamTypeString as i32,
        WidgetParamType::Color => proto::WidgetParamTypeProto::WidgetParamTypeColor as i32,
        WidgetParamType::Enum => proto::WidgetParamTypeProto::WidgetParamTypeEnum as i32,
    }
}

/// Convert a proto WidgetParamTypeProto i32 to scene WidgetParamType.
pub fn proto_to_widget_param_type(v: i32) -> WidgetParamType {
    match proto::WidgetParamTypeProto::try_from(v)
        .unwrap_or(proto::WidgetParamTypeProto::WidgetParamTypeUnspecified)
    {
        proto::WidgetParamTypeProto::WidgetParamTypeF32 => WidgetParamType::F32,
        proto::WidgetParamTypeProto::WidgetParamTypeString => WidgetParamType::String,
        proto::WidgetParamTypeProto::WidgetParamTypeColor => WidgetParamType::Color,
        proto::WidgetParamTypeProto::WidgetParamTypeEnum => WidgetParamType::Enum,
        proto::WidgetParamTypeProto::WidgetParamTypeUnspecified => WidgetParamType::F32,
    }
}

/// Convert a scene WidgetParameterValue to a proto WidgetParameterValueProto.
pub fn widget_param_value_to_proto(
    name: &str,
    v: &WidgetParameterValue,
) -> proto::WidgetParameterValueProto {
    use proto::widget_parameter_value_proto::Value;
    let value = match v {
        WidgetParameterValue::F32(f) => Some(Value::F32Value(*f)),
        WidgetParameterValue::String(s) => Some(Value::StringValue(s.clone())),
        WidgetParameterValue::Color(c) => Some(Value::ColorValue(proto::Rgba {
            r: c.r,
            g: c.g,
            b: c.b,
            a: c.a,
        })),
        WidgetParameterValue::Enum(e) => Some(Value::EnumValue(e.clone())),
    };
    proto::WidgetParameterValueProto {
        param_name: name.to_string(),
        value,
    }
}

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

/// Convert a scene WidgetParamConstraints to proto WidgetParamConstraintsProto.
pub fn widget_param_constraints_to_proto(
    c: &WidgetParamConstraints,
) -> proto::WidgetParamConstraintsProto {
    proto::WidgetParamConstraintsProto {
        f32_min: c.f32_min.unwrap_or(0.0),
        has_f32_min: c.f32_min.is_some(),
        f32_max: c.f32_max.unwrap_or(0.0),
        has_f32_max: c.f32_max.is_some(),
        string_max_bytes: c.string_max_bytes.unwrap_or(0),
        enum_allowed_values: c.enum_allowed_values.clone(),
    }
}

/// Convert a proto WidgetParamConstraintsProto to scene WidgetParamConstraints.
pub fn proto_to_widget_param_constraints(
    p: &proto::WidgetParamConstraintsProto,
) -> WidgetParamConstraints {
    WidgetParamConstraints {
        f32_min: if p.has_f32_min { Some(p.f32_min) } else { None },
        f32_max: if p.has_f32_max { Some(p.f32_max) } else { None },
        string_max_bytes: if p.string_max_bytes != 0 {
            Some(p.string_max_bytes)
        } else {
            None
        },
        enum_allowed_values: p.enum_allowed_values.clone(),
    }
}

/// Convert a scene WidgetParameterDeclaration to proto WidgetParameterDeclarationProto.
pub fn widget_param_decl_to_proto(
    d: &WidgetParameterDeclaration,
) -> proto::WidgetParameterDeclarationProto {
    proto::WidgetParameterDeclarationProto {
        name: d.name.clone(),
        param_type: widget_param_type_to_proto(d.param_type),
        default_value: Some(widget_param_value_to_proto(&d.name, &d.default_value)),
        constraints: d
            .constraints
            .as_ref()
            .map(widget_param_constraints_to_proto),
    }
}

/// Convert a proto WidgetParameterDeclarationProto to scene WidgetParameterDeclaration.
pub fn proto_to_widget_param_decl(
    p: &proto::WidgetParameterDeclarationProto,
) -> Option<WidgetParameterDeclaration> {
    let default_proto = p.default_value.as_ref()?;
    let (_, default_value) = proto_to_widget_param_value(default_proto)?;
    let constraints = p
        .constraints
        .as_ref()
        .map(proto_to_widget_param_constraints);
    Some(WidgetParameterDeclaration {
        name: p.name.clone(),
        param_type: proto_to_widget_param_type(p.param_type),
        default_value,
        constraints,
    })
}

/// Convert a scene WidgetBindingMapping to proto WidgetBindingMappingProto.
pub fn widget_binding_mapping_to_proto(
    m: &WidgetBindingMapping,
) -> proto::WidgetBindingMappingProto {
    use proto::widget_binding_mapping_proto::Mapping;
    let mapping = match m {
        WidgetBindingMapping::Linear { attr_min, attr_max } => {
            Some(Mapping::Linear(proto::WidgetLinearMappingProto {
                attr_min: *attr_min,
                attr_max: *attr_max,
            }))
        }
        WidgetBindingMapping::Direct => Some(Mapping::Direct(true)),
        WidgetBindingMapping::Discrete { value_map } => {
            Some(Mapping::Discrete(proto::WidgetDiscreteMappingProto {
                value_map: value_map
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            }))
        }
    };
    proto::WidgetBindingMappingProto { mapping }
}

/// Convert a proto WidgetBindingMappingProto to scene WidgetBindingMapping.
pub fn proto_to_widget_binding_mapping(
    p: &proto::WidgetBindingMappingProto,
) -> Option<WidgetBindingMapping> {
    use proto::widget_binding_mapping_proto::Mapping;
    match p.mapping.as_ref()? {
        Mapping::Linear(l) => Some(WidgetBindingMapping::Linear {
            attr_min: l.attr_min,
            attr_max: l.attr_max,
        }),
        Mapping::Direct(_) => Some(WidgetBindingMapping::Direct),
        Mapping::Discrete(d) => Some(WidgetBindingMapping::Discrete {
            value_map: d
                .value_map
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }),
    }
}

/// Convert a scene WidgetBinding to proto WidgetBindingProto.
pub fn widget_binding_to_proto(b: &WidgetBinding) -> proto::WidgetBindingProto {
    proto::WidgetBindingProto {
        param: b.param.clone(),
        target_element: b.target_element.clone(),
        target_attribute: b.target_attribute.clone(),
        mapping: Some(widget_binding_mapping_to_proto(&b.mapping)),
    }
}

/// Convert a proto WidgetBindingProto to scene WidgetBinding.
pub fn proto_to_widget_binding(p: &proto::WidgetBindingProto) -> Option<WidgetBinding> {
    let mapping = proto_to_widget_binding_mapping(p.mapping.as_ref()?)?;
    Some(WidgetBinding {
        param: p.param.clone(),
        target_element: p.target_element.clone(),
        target_attribute: p.target_attribute.clone(),
        mapping,
    })
}

/// Convert a scene WidgetSvgLayer to proto WidgetSvgLayerProto.
pub fn widget_svg_layer_to_proto(l: &WidgetSvgLayer) -> proto::WidgetSvgLayerProto {
    proto::WidgetSvgLayerProto {
        svg_file: l.svg_file.clone(),
        bindings: l.bindings.iter().map(widget_binding_to_proto).collect(),
    }
}

/// Convert a proto WidgetSvgLayerProto to scene WidgetSvgLayer.
pub fn proto_to_widget_svg_layer(p: &proto::WidgetSvgLayerProto) -> WidgetSvgLayer {
    WidgetSvgLayer {
        svg_file: p.svg_file.clone(),
        bindings: p
            .bindings
            .iter()
            .filter_map(proto_to_widget_binding)
            .collect(),
    }
}

/// Convert a scene WidgetDefinition to proto WidgetDefinitionProto.
pub fn widget_definition_to_proto(d: &WidgetDefinition) -> proto::WidgetDefinitionProto {
    let parameter_schema = Some(proto::WidgetParameterSchemaProto {
        parameters: d
            .parameter_schema
            .iter()
            .map(widget_param_decl_to_proto)
            .collect(),
    });
    let layers = d.layers.iter().map(widget_svg_layer_to_proto).collect();
    let default_geometry_policy = Some(geometry_policy_to_proto(&d.default_geometry_policy));
    let default_rendering_policy = Some(rendering_policy_to_proto(&d.default_rendering_policy));

    let (default_contention_policy, stack_max_depth, merge_max_keys) =
        match d.default_contention_policy {
            ContentionPolicy::LatestWins => (
                proto::ContentionPolicyProto::ContentionPolicyLatestWins as i32,
                0u32,
                0u32,
            ),
            ContentionPolicy::Replace => (
                proto::ContentionPolicyProto::ContentionPolicyReplace as i32,
                0u32,
                0u32,
            ),
            ContentionPolicy::Stack { max_depth } => (
                proto::ContentionPolicyProto::ContentionPolicyStack as i32,
                u32::from(max_depth),
                0u32,
            ),
            ContentionPolicy::MergeByKey { max_keys } => (
                proto::ContentionPolicyProto::ContentionPolicyMergeByKey as i32,
                0u32,
                u32::from(max_keys),
            ),
        };

    proto::WidgetDefinitionProto {
        id: d.id.clone(),
        name: d.name.clone(),
        description: d.description.clone(),
        parameter_schema,
        layers,
        default_geometry_policy,
        default_rendering_policy,
        default_contention_policy,
        ephemeral: d.ephemeral,
        stack_max_depth,
        merge_max_keys,
    }
}

/// Convert a proto WidgetDefinitionProto to scene WidgetDefinition.
pub fn proto_to_widget_definition(p: &proto::WidgetDefinitionProto) -> WidgetDefinition {
    let parameter_schema = p
        .parameter_schema
        .as_ref()
        .map(|s| {
            s.parameters
                .iter()
                .filter_map(proto_to_widget_param_decl)
                .collect()
        })
        .unwrap_or_default();

    let layers = p.layers.iter().map(proto_to_widget_svg_layer).collect();

    let default_geometry_policy = p
        .default_geometry_policy
        .as_ref()
        .and_then(proto_to_geometry_policy)
        .unwrap_or(GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.1,
            height_pct: 0.1,
        });

    let default_rendering_policy = p
        .default_rendering_policy
        .as_ref()
        .map(proto_to_rendering_policy)
        .unwrap_or_default();

    let default_contention_policy =
        match proto::ContentionPolicyProto::try_from(p.default_contention_policy)
            .unwrap_or(proto::ContentionPolicyProto::ContentionPolicyLatestWins)
        {
            proto::ContentionPolicyProto::ContentionPolicyLatestWins
            | proto::ContentionPolicyProto::ContentionPolicyUnspecified => {
                ContentionPolicy::LatestWins
            }
            proto::ContentionPolicyProto::ContentionPolicyReplace => ContentionPolicy::Replace,
            proto::ContentionPolicyProto::ContentionPolicyStack => ContentionPolicy::Stack {
                max_depth: if p.stack_max_depth > 0 {
                    p.stack_max_depth as u8
                } else {
                    8
                },
            },
            proto::ContentionPolicyProto::ContentionPolicyMergeByKey => {
                ContentionPolicy::MergeByKey {
                    max_keys: if p.merge_max_keys > 0 {
                        p.merge_max_keys as u8
                    } else {
                        16
                    },
                }
            }
        };

    WidgetDefinition {
        id: p.id.clone(),
        name: p.name.clone(),
        description: p.description.clone(),
        parameter_schema,
        layers,
        default_geometry_policy,
        default_rendering_policy,
        default_contention_policy,
        max_publishers: WidgetDefinition::default_max_publishers(),
        ephemeral: p.ephemeral,
        hover_behavior: None,
    }
}

/// Convert a scene WidgetInstance to proto WidgetInstanceProto.
pub fn widget_instance_to_proto(i: &WidgetInstance) -> proto::WidgetInstanceProto {
    let geometry_override = i.geometry_override.as_ref().map(geometry_policy_to_proto);

    let contention_override = i.contention_override.as_ref().map(|cp| match cp {
        ContentionPolicy::LatestWins => {
            proto::ContentionPolicyProto::ContentionPolicyLatestWins as i32
        }
        ContentionPolicy::Replace => proto::ContentionPolicyProto::ContentionPolicyReplace as i32,
        ContentionPolicy::Stack { .. } => {
            proto::ContentionPolicyProto::ContentionPolicyStack as i32
        }
        ContentionPolicy::MergeByKey { .. } => {
            proto::ContentionPolicyProto::ContentionPolicyMergeByKey as i32
        }
    });

    let current_params = i
        .current_params
        .iter()
        .map(|(name, val)| widget_param_value_to_proto(name, val))
        .collect();

    proto::WidgetInstanceProto {
        widget_type_name: i.widget_type_name.clone(),
        tab_id: i.tab_id.to_bytes_le().to_vec(),
        geometry_override,
        contention_override: contention_override
            .unwrap_or(proto::ContentionPolicyProto::ContentionPolicyUnspecified as i32),
        instance_name: i.instance_name.clone(),
        current_params,
    }
}

/// Convert a proto WidgetInstanceProto to scene WidgetInstance.
pub fn proto_to_widget_instance(p: &proto::WidgetInstanceProto) -> Option<WidgetInstance> {
    let tab_id = if p.tab_id.is_empty() {
        SceneId::null()
    } else {
        SceneId::from_bytes_le(&p.tab_id)?
    };

    let geometry_override = p
        .geometry_override
        .as_ref()
        .and_then(proto_to_geometry_policy);

    let contention_override = if p.contention_override
        == proto::ContentionPolicyProto::ContentionPolicyUnspecified as i32
    {
        None
    } else {
        Some(
            match proto::ContentionPolicyProto::try_from(p.contention_override)
                .unwrap_or(proto::ContentionPolicyProto::ContentionPolicyLatestWins)
            {
                proto::ContentionPolicyProto::ContentionPolicyLatestWins
                | proto::ContentionPolicyProto::ContentionPolicyUnspecified => {
                    ContentionPolicy::LatestWins
                }
                proto::ContentionPolicyProto::ContentionPolicyReplace => ContentionPolicy::Replace,
                proto::ContentionPolicyProto::ContentionPolicyStack => {
                    ContentionPolicy::Stack { max_depth: 8 }
                }
                proto::ContentionPolicyProto::ContentionPolicyMergeByKey => {
                    ContentionPolicy::MergeByKey { max_keys: 16 }
                }
            },
        )
    };

    let current_params = p
        .current_params
        .iter()
        .filter_map(proto_to_widget_param_value)
        .collect();

    Some(WidgetInstance {
        id: SceneId::null(),
        widget_type_name: p.widget_type_name.clone(),
        tab_id,
        geometry_override,
        contention_override,
        instance_name: p.instance_name.clone(),
        current_params,
    })
}

/// Convert a scene WidgetPublishRecord to proto WidgetPublishRecordProto.
pub fn widget_publish_record_to_proto(r: &WidgetPublishRecord) -> proto::WidgetPublishRecordProto {
    let params = r
        .params
        .iter()
        .map(|(name, val)| widget_param_value_to_proto(name, val))
        .collect();
    proto::WidgetPublishRecordProto {
        widget_name: r.widget_name.clone(),
        publisher_namespace: r.publisher_namespace.clone(),
        params,
        published_at_wall_us: r.published_at_wall_us,
        merge_key: r.merge_key.clone().unwrap_or_default(),
        expires_at_wall_us: r.expires_at_wall_us.unwrap_or(0),
        transition_ms: r.transition_ms,
    }
}

/// Convert a proto WidgetPublishRecordProto to scene WidgetPublishRecord.
pub fn proto_to_widget_publish_record(p: &proto::WidgetPublishRecordProto) -> WidgetPublishRecord {
    let params = p
        .params
        .iter()
        .filter_map(proto_to_widget_param_value)
        .collect();
    WidgetPublishRecord {
        widget_name: p.widget_name.clone(),
        publisher_namespace: p.publisher_namespace.clone(),
        params,
        published_at_wall_us: p.published_at_wall_us,
        merge_key: if p.merge_key.is_empty() {
            None
        } else {
            Some(p.merge_key.clone())
        },
        expires_at_wall_us: if p.expires_at_wall_us == 0 {
            None
        } else {
            Some(p.expires_at_wall_us)
        },
        transition_ms: p.transition_ms,
    }
}

/// Convert a scene WidgetRegistrySnapshot to proto WidgetRegistrySnapshotProto.
pub fn widget_registry_snapshot_to_proto(
    snap: &WidgetRegistrySnapshot,
) -> proto::WidgetRegistrySnapshotProto {
    proto::WidgetRegistrySnapshotProto {
        widget_types: snap
            .widget_types
            .iter()
            .map(widget_definition_to_proto)
            .collect(),
        widget_instances: snap
            .widget_instances
            .iter()
            .map(widget_instance_to_proto)
            .collect(),
        active_publishes: snap
            .active_publishes
            .iter()
            .map(widget_publish_record_to_proto)
            .collect(),
    }
}

/// Convert a proto WidgetRegistrySnapshotProto to scene WidgetRegistrySnapshot.
pub fn proto_to_widget_registry_snapshot(
    p: &proto::WidgetRegistrySnapshotProto,
) -> WidgetRegistrySnapshot {
    WidgetRegistrySnapshot {
        widget_types: p
            .widget_types
            .iter()
            .map(proto_to_widget_definition)
            .collect(),
        widget_instances: p
            .widget_instances
            .iter()
            .filter_map(proto_to_widget_instance)
            .collect(),
        active_publishes: p
            .active_publishes
            .iter()
            .map(proto_to_widget_publish_record)
            .collect(),
    }
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

    // ── Widget proto round-trips ──────────────────────────────────────────────

    fn make_widget_definition() -> WidgetDefinition {
        WidgetDefinition {
            id: "gauge".to_string(),
            name: "Gauge".to_string(),
            description: "A gauge widget".to_string(),
            parameter_schema: vec![
                WidgetParameterDeclaration {
                    name: "level".to_string(),
                    param_type: WidgetParamType::F32,
                    default_value: WidgetParameterValue::F32(0.0),
                    constraints: Some(WidgetParamConstraints {
                        f32_min: Some(0.0),
                        f32_max: Some(1.0),
                        ..Default::default()
                    }),
                },
                WidgetParameterDeclaration {
                    name: "label".to_string(),
                    param_type: WidgetParamType::String,
                    default_value: WidgetParameterValue::String("".to_string()),
                    constraints: None,
                },
                WidgetParameterDeclaration {
                    name: "fill_color".to_string(),
                    param_type: WidgetParamType::Color,
                    default_value: WidgetParameterValue::Color(Rgba::new(0.0, 1.0, 0.0, 1.0)),
                    constraints: None,
                },
                WidgetParameterDeclaration {
                    name: "severity".to_string(),
                    param_type: WidgetParamType::Enum,
                    default_value: WidgetParameterValue::Enum("info".to_string()),
                    constraints: Some(WidgetParamConstraints {
                        enum_allowed_values: vec![
                            "info".to_string(),
                            "warning".to_string(),
                            "error".to_string(),
                        ],
                        ..Default::default()
                    }),
                },
            ],
            layers: vec![WidgetSvgLayer {
                svg_file: "fill.svg".to_string(),
                bindings: vec![
                    WidgetBinding {
                        param: "level".to_string(),
                        target_element: "bar".to_string(),
                        target_attribute: "height".to_string(),
                        mapping: WidgetBindingMapping::Linear {
                            attr_min: 0.0,
                            attr_max: 200.0,
                        },
                    },
                    WidgetBinding {
                        param: "label".to_string(),
                        target_element: "label-text".to_string(),
                        target_attribute: "text-content".to_string(),
                        mapping: WidgetBindingMapping::Direct,
                    },
                    WidgetBinding {
                        param: "severity".to_string(),
                        target_element: "indicator".to_string(),
                        target_attribute: "fill".to_string(),
                        mapping: WidgetBindingMapping::Discrete {
                            value_map: [
                                ("info".to_string(), "#00ff00".to_string()),
                                ("warning".to_string(), "#ffff00".to_string()),
                                ("error".to_string(), "#ff0000".to_string()),
                            ]
                            .iter()
                            .cloned()
                            .collect(),
                        },
                    },
                ],
            }],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.8,
                y_pct: 0.1,
                width_pct: 0.15,
                height_pct: 0.25,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: ContentionPolicy::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: false,
            hover_behavior: None,
        }
    }

    #[test]
    fn widget_definition_proto_round_trip() {
        let original = make_widget_definition();
        let proto = widget_definition_to_proto(&original);
        let restored = proto_to_widget_definition(&proto);

        assert_eq!(restored.id, original.id);
        assert_eq!(restored.name, original.name);
        assert_eq!(restored.description, original.description);
        assert_eq!(restored.ephemeral, original.ephemeral);
        assert_eq!(
            restored.parameter_schema.len(),
            original.parameter_schema.len(),
            "parameter schema length must survive round-trip"
        );

        // Check each parameter declaration — including constraints.
        // make_widget_definition sets f32_min: Some(0.0) on "level", which
        // exercises the has_f32_min sentinel fix (0.0 must not be confused with None).
        for (orig_decl, rest_decl) in original
            .parameter_schema
            .iter()
            .zip(restored.parameter_schema.iter())
        {
            assert_eq!(rest_decl.name, orig_decl.name);
            assert_eq!(rest_decl.param_type, orig_decl.param_type);
            assert_eq!(rest_decl.default_value, orig_decl.default_value);
            assert_eq!(
                rest_decl.constraints, orig_decl.constraints,
                "constraints for '{}' must survive round-trip (including Some(0.0))",
                orig_decl.name
            );
        }

        assert_eq!(restored.layers.len(), original.layers.len());
        assert_eq!(restored.layers[0].svg_file, original.layers[0].svg_file);
        assert_eq!(
            restored.layers[0].bindings.len(),
            original.layers[0].bindings.len()
        );
    }

    #[test]
    fn widget_constraints_f32_zero_round_trip() {
        // Regression: Some(0.0) must not be confused with None after encode/decode.
        // Without has_f32_min/has_f32_max booleans, 0.0 is indistinguishable
        // from the proto default (also 0.0) and would be decoded as None.
        let constraints = WidgetParamConstraints {
            f32_min: Some(0.0),
            f32_max: Some(0.0),
            ..Default::default()
        };
        let proto = widget_param_constraints_to_proto(&constraints);
        let restored = proto_to_widget_param_constraints(&proto);
        assert_eq!(
            restored.f32_min,
            Some(0.0),
            "f32_min: Some(0.0) must survive round-trip"
        );
        assert_eq!(
            restored.f32_max,
            Some(0.0),
            "f32_max: Some(0.0) must survive round-trip"
        );
    }

    #[test]
    fn widget_constraints_none_f32_round_trip() {
        // None f32 constraints must survive as None (not Some(0.0)).
        let constraints = WidgetParamConstraints::default();
        let proto = widget_param_constraints_to_proto(&constraints);
        let restored = proto_to_widget_param_constraints(&proto);
        assert_eq!(
            restored.f32_min, None,
            "None f32_min must survive round-trip"
        );
        assert_eq!(
            restored.f32_max, None,
            "None f32_max must survive round-trip"
        );
    }

    #[test]
    fn widget_definition_stack_contention_round_trip() {
        // Verify stack_max_depth survives proto round-trip (previously hard-coded to 8).
        let mut def = make_widget_definition();
        def.default_contention_policy = ContentionPolicy::Stack { max_depth: 5 };
        let proto = widget_definition_to_proto(&def);
        let restored = proto_to_widget_definition(&proto);
        assert_eq!(
            restored.default_contention_policy,
            ContentionPolicy::Stack { max_depth: 5 },
            "Stack {{ max_depth }} must survive proto round-trip"
        );
    }

    #[test]
    fn widget_definition_merge_contention_round_trip() {
        // Verify merge_max_keys survives proto round-trip (previously hard-coded to 16).
        let mut def = make_widget_definition();
        def.default_contention_policy = ContentionPolicy::MergeByKey { max_keys: 12 };
        let proto = widget_definition_to_proto(&def);
        let restored = proto_to_widget_definition(&proto);
        assert_eq!(
            restored.default_contention_policy,
            ContentionPolicy::MergeByKey { max_keys: 12 },
            "MergeByKey {{ max_keys }} must survive proto round-trip"
        );
    }

    #[test]
    fn widget_parameter_value_f32_round_trip() {
        let val = WidgetParameterValue::F32(0.75);
        let proto = widget_param_value_to_proto("level", &val);
        let (name, restored) = proto_to_widget_param_value(&proto).unwrap();
        assert_eq!(name, "level");
        assert_eq!(restored, val);
    }

    #[test]
    fn widget_parameter_value_string_round_trip() {
        let val = WidgetParameterValue::String("hello world".to_string());
        let proto = widget_param_value_to_proto("label", &val);
        let (name, restored) = proto_to_widget_param_value(&proto).unwrap();
        assert_eq!(name, "label");
        assert_eq!(restored, val);
    }

    #[test]
    fn widget_parameter_value_color_round_trip() {
        let val = WidgetParameterValue::Color(Rgba::new(0.5, 0.25, 0.75, 1.0));
        let proto = widget_param_value_to_proto("fill_color", &val);
        let (name, restored) = proto_to_widget_param_value(&proto).unwrap();
        assert_eq!(name, "fill_color");
        assert_eq!(restored, val);
    }

    #[test]
    fn widget_parameter_value_enum_round_trip() {
        let val = WidgetParameterValue::Enum("warning".to_string());
        let proto = widget_param_value_to_proto("severity", &val);
        let (name, restored) = proto_to_widget_param_value(&proto).unwrap();
        assert_eq!(name, "severity");
        assert_eq!(restored, val);
    }

    #[test]
    fn widget_binding_mapping_linear_round_trip() {
        let mapping = WidgetBindingMapping::Linear {
            attr_min: 0.0,
            attr_max: 200.0,
        };
        let proto = widget_binding_mapping_to_proto(&mapping);
        let restored = proto_to_widget_binding_mapping(&proto).unwrap();
        assert_eq!(restored, mapping);
    }

    #[test]
    fn widget_binding_mapping_direct_round_trip() {
        let mapping = WidgetBindingMapping::Direct;
        let proto = widget_binding_mapping_to_proto(&mapping);
        let restored = proto_to_widget_binding_mapping(&proto).unwrap();
        assert_eq!(restored, mapping);
    }

    #[test]
    fn widget_binding_mapping_discrete_round_trip() {
        let mapping = WidgetBindingMapping::Discrete {
            value_map: [
                ("info".to_string(), "#00ff00".to_string()),
                ("error".to_string(), "#ff0000".to_string()),
            ]
            .iter()
            .cloned()
            .collect(),
        };
        let proto = widget_binding_mapping_to_proto(&mapping);
        let restored = proto_to_widget_binding_mapping(&proto).unwrap();
        assert_eq!(restored, mapping);
    }

    #[test]
    fn widget_publish_record_proto_round_trip() {
        let params: std::collections::HashMap<String, WidgetParameterValue> = [
            ("level".to_string(), WidgetParameterValue::F32(0.8)),
            (
                "label".to_string(),
                WidgetParameterValue::String("CPU".to_string()),
            ),
        ]
        .iter()
        .cloned()
        .collect();

        let record = WidgetPublishRecord {
            widget_name: "gauge".to_string(),
            publisher_namespace: "agent.test".to_string(),
            params: params.clone(),
            published_at_wall_us: 1_700_000_000_000_000,
            merge_key: Some("cpu-key".to_string()),
            expires_at_wall_us: Some(1_700_000_060_000_000),
            transition_ms: 300,
        };

        let proto = widget_publish_record_to_proto(&record);
        let restored = proto_to_widget_publish_record(&proto);

        assert_eq!(restored.widget_name, record.widget_name);
        assert_eq!(restored.publisher_namespace, record.publisher_namespace);
        assert_eq!(restored.published_at_wall_us, record.published_at_wall_us);
        assert_eq!(restored.merge_key, record.merge_key);
        assert_eq!(restored.expires_at_wall_us, record.expires_at_wall_us);
        assert_eq!(restored.transition_ms, record.transition_ms);
        assert_eq!(restored.params.len(), record.params.len());
    }

    #[test]
    fn widget_registry_snapshot_round_trip() {
        let def = make_widget_definition();
        let tab_id = SceneId::new();
        let instance = WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "gauge".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: "gauge".to_string(),
            current_params: [("level".to_string(), WidgetParameterValue::F32(0.5))]
                .iter()
                .cloned()
                .collect(),
        };

        let snapshot = WidgetRegistrySnapshot {
            widget_types: vec![def],
            widget_instances: vec![instance],
            active_publishes: vec![],
        };

        let proto = widget_registry_snapshot_to_proto(&snapshot);
        let restored = proto_to_widget_registry_snapshot(&proto);

        assert_eq!(restored.widget_types.len(), 1);
        assert_eq!(restored.widget_types[0].id, "gauge");
        assert_eq!(restored.widget_instances.len(), 1);
        assert_eq!(restored.widget_instances[0].instance_name, "gauge");
        assert_eq!(restored.widget_instances[0].tab_id, tab_id);
    }

    #[test]
    fn widget_registry_in_scene_graph() {
        // Verify WidgetRegistry is accessible from SceneGraph root.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let def = make_widget_definition();
        scene.widget_registry.register_definition(def);
        assert!(
            scene.widget_registry.get_definition("gauge").is_some(),
            "widget definition must be retrievable from SceneGraph"
        );
    }

    #[test]
    fn widget_registry_in_scene_snapshot() {
        // Verify SceneGraphSnapshot includes widget_registry field.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let def = make_widget_definition();
        scene.widget_registry.register_definition(def);

        let snapshot = scene.take_snapshot(1_000_000, 1_000_000);
        assert_eq!(
            snapshot.widget_registry.widget_types.len(),
            1,
            "snapshot widget_registry must include all registered definitions"
        );
        assert!(
            snapshot.widget_registry.widget_types.contains_key("gauge"),
            "snapshot widget_registry must contain 'gauge' key"
        );
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
}
