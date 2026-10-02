//! MCP lifecycle + introspection payloads (`initialize`, `tools/list`).
//!
//! The server is a bare JSON-RPC method router; standard MCP clients discover
//! the available tools and their input schemas via `tools/list`, and negotiate
//! the protocol version via `initialize`. Without these, an LLM cannot
//! introspect any tool and is wholly dependent on out-of-band skill docs.
//!
//! Each tool's `inputSchema` is **derived** from the matching `*Params` struct
//! in [`crate::tools`] via [`schemars`] (`#[derive(JsonSchema)]`), so the
//! introspection contract cannot drift from the shape the runtime actually
//! deserializes. Adding, renaming, or re-typing a `*Params` field flows through
//! to `tools/list` automatically — no hand-editing here. Only the tool *name*
//! and human-facing *description* live in this file (they have no home on the
//! struct); everything else — property names, types, and required/optional —
//! comes from the derive.

use crate::tools::{
    ClearWidgetParams, CreateTabParams, CreateTileParams, DismissParams, InjectComposerPasteParams,
    ListElementsParams, ListSceneParams, ListWidgetsParams, ListZonesParams,
    PortalProjectionAcknowledgeInputParams, PortalProjectionAttachParams,
    PortalProjectionCleanupParams, PortalProjectionDetachParams,
    PortalProjectionGetPendingInputParams, PortalProjectionListParams,
    PortalProjectionPublishParams, PortalProjectionPublishStatusParams, PublishToElementParams,
    PublishToWidgetParams, PublishToZoneParams, RegisterWidgetAssetParams, SetContentParams,
};
use schemars::JsonSchema;
use schemars::r#gen::SchemaSettings;
use serde_json::{Value, json};

/// MCP protocol revision advertised by `initialize`.
///
/// The runtime implements the JSON-RPC tool surface compatible with this
/// revision of the Model Context Protocol.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Build the `initialize` handshake result.
///
/// Returns the protocol version, server identity, and declared capabilities.
/// The runtime exposes a static tool surface, so `tools.listChanged` is
/// `false` (the list never changes mid-session).
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "serverInfo": {
            "name": "tze_hud_mcp",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": {
            "tools": { "listChanged": false }
        },
    })
}

/// Build the `tools/list` result: `{ "tools": [ { name, description,
/// inputSchema }, ... ] }`.
pub fn tools_list_result() -> Value {
    json!({ "tools": tool_descriptors() })
}

/// Derive a JSON Schema `inputSchema` object for a `*Params` struct.
///
/// The schema is generated with `inline_subschemas` so nested params (bounds,
/// color, …) are emitted inline instead of behind `$ref`/`definitions`, keeping
/// the payload self-contained the way MCP clients expect. The generator's
/// document-level `$schema`/`title` metadata is stripped so the result is a
/// bare `{ "type": "object", "properties": {…}, "required": […] }` object —
/// identical in shape to the descriptors this crate emitted by hand before the
/// derive.
///
/// `properties` and `required` are always present (defaulting to `{}` / `[]`);
/// schemars omits them for empty or all-required structs, but the pre-derive
/// wire contract always carried both keys, and some MCP clients index them
/// unconditionally.
fn input_schema_for<T: JsonSchema>() -> Value {
    let generator = SchemaSettings::draft07()
        .with(|s| s.inline_subschemas = true)
        .into_generator();
    let root = generator.into_root_schema_for::<T>();
    let mut schema =
        serde_json::to_value(&root).expect("schemars root schema must serialize to a JSON value");
    if let Value::Object(map) = &mut schema {
        // Document-level metadata that has no meaning inside a tool inputSchema.
        map.remove("$schema");
        map.remove("title");
        // Preserve the always-present `properties`/`required` wire shape.
        map.entry("properties").or_insert_with(|| json!({}));
        map.entry("required").or_insert_with(|| json!([]));
    }
    schema
}

/// A single tool descriptor whose `inputSchema` is derived from `T`.
fn tool<T: JsonSchema>(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema_for::<T>(),
    })
}

/// Full ordered list of tool descriptors. Order mirrors the dispatch router in
/// [`crate::server`] for easy cross-checking. Names and descriptions are the
/// only hand-maintained fields; each `inputSchema` is derived from the tool's
/// `*Params` struct so it stays in lockstep with the deserialized shape.
fn tool_descriptors() -> Vec<Value> {
    vec![
        tool::<CreateTabParams>("create_tab", "Create a new tab in the scene."),
        tool::<CreateTileParams>(
            "create_tile",
            "Create a tile within a tab and grant it a lease.",
        ),
        tool::<SetContentParams>("set_content", "Set markdown text content on a tile."),
        tool::<DismissParams>("dismiss", "Delete a tile and release its lease."),
        tool::<PublishToZoneParams>("publish_to_zone", "Publish content to a named zone."),
        tool::<ListZonesParams>("list_zones", "List all registered zones."),
        tool::<ListSceneParams>("list_scene", "List the scene's tabs."),
        tool::<ListElementsParams>(
            "list_elements",
            "List addressable elements (tiles, zones, widgets).",
        ),
        tool::<PublishToWidgetParams>(
            "publish_to_widget",
            "Publish typed parameter values to a widget instance.",
        ),
        tool::<ListWidgetsParams>(
            "list_widgets",
            "List registered widget types and instances.",
        ),
        tool::<ClearWidgetParams>(
            "clear_widget",
            "Clear a namespace's publications from a widget instance.",
        ),
        tool::<RegisterWidgetAssetParams>(
            "register_widget_asset",
            "Register (or dedup-preflight) an SVG asset for a widget type.",
        ),
        tool::<PublishToElementParams>(
            "publish_to_element",
            "Publish content to a tile, zone, or widget by stable element UUID.",
        ),
        tool::<InjectComposerPasteParams>(
            "inject_composer_paste",
            "Inject text into the active composer draft buffer.",
        ),
        // ── Portal projection tools (cooperative HUD self-projection) ─────────
        tool::<PortalProjectionListParams>(
            "portal_projection_list",
            "List bounded, content-free summaries of the resident caller's projections.",
        ),
        tool::<PortalProjectionAttachParams>(
            "portal_projection_attach",
            "Attach a new cooperative projection session to the in-process authority.",
        ),
        tool::<PortalProjectionPublishParams>(
            "portal_projection_publish",
            "Append output text to an existing projection transcript.",
        ),
        tool::<PortalProjectionPublishStatusParams>(
            "portal_projection_publish_status",
            "Publish a lifecycle status to an existing projection session.",
        ),
        tool::<PortalProjectionGetPendingInputParams>(
            "portal_projection_get_pending_input",
            "Drain HUD-originated pending input for a projection session.",
        ),
        tool::<PortalProjectionAcknowledgeInputParams>(
            "portal_projection_acknowledge_input",
            "Acknowledge a delivered input item for a projection session.",
        ),
        tool::<PortalProjectionDetachParams>(
            "portal_projection_detach",
            "Detach a projection session, purging its private state.",
        ),
        tool::<PortalProjectionCleanupParams>(
            "portal_projection_cleanup",
            "Cleanup a projection session via owner or operator authority.",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialized byte size of a whole tools/list payload subset. This is the
    /// wire cost every attached MCP session pays once in its context window, so
    /// the budget guards below lock in the token-efficiency work (hud-hzsgp) and
    /// catch a regression back toward the pre-slimming verbosity.
    ///
    /// The `portal_projection_*` schemas — the projection self-attach surface —
    /// were slimmed from ~9,336 to ~6,836 serialized bytes by removing per-field
    /// doc-comment redundancy while keeping every property, enum value, default,
    /// and bound. The budgets carry modest headroom for small future additions;
    /// tighten or raise them deliberately (never to smuggle prose back in).
    #[test]
    fn tools_list_stays_within_token_budget() {
        let list = tools_list_result();
        let tools = list["tools"].as_array().unwrap();
        let full_bytes = serde_json::to_string(&list).unwrap().len();
        let portal_bytes: usize = tools
            .iter()
            .filter(|t| {
                t["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with("portal_projection_"))
            })
            .map(|t| serde_json::to_string(t).unwrap().len())
            .sum();

        assert!(
            portal_bytes <= 7_500,
            "portal_projection_* tools/list bytes {portal_bytes} exceeds budget 7500 \
             (pre-slimming was 9336); did a verbose field doc creep back in?"
        );
        assert!(
            full_bytes <= 20_000,
            "full tools/list bytes {full_bytes} exceeds budget 20000 \
             (pre-slimming was 21549)"
        );
    }

    /// Fetch a single tool's `inputSchema` from the live `tools/list` payload.
    fn input_schema(tool_name: &str) -> Value {
        let list = tools_list_result();
        let tools = list["tools"].as_array().expect("tools must be an array");
        let tool = tools
            .iter()
            .find(|t| t["name"] == tool_name)
            .unwrap_or_else(|| panic!("tool {tool_name} not present in tools/list"));
        tool["inputSchema"].clone()
    }

    /// The property names declared for a tool's inputSchema.
    fn prop_names(schema: &Value) -> Vec<String> {
        schema["properties"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The `required` list for a tool's inputSchema.
    fn required(schema: &Value) -> Vec<String> {
        schema["required"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every derived inputSchema is a JSON Schema object, and the tool set /
    /// order matches the dispatch router. Guards against an accidental empty or
    /// malformed derive.
    #[test]
    fn every_tool_has_object_input_schema() {
        let list = tools_list_result();
        let tools = list["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 22, "expected 22 MCP tools");
        for t in tools {
            assert!(t["name"].is_string(), "tool missing name: {t:?}");
            assert!(
                t["description"].is_string(),
                "tool missing description: {t:?}"
            );
            assert_eq!(
                t["inputSchema"]["type"], "object",
                "tool {} inputSchema is not an object",
                t["name"]
            );
        }
    }

    /// A representative tool's derived schema carries every field the `*Params`
    /// struct declares, with the correct required/optional split — the wire
    /// contract MCP clients depend on.
    #[test]
    fn create_tile_schema_matches_params_struct() {
        let schema = input_schema("create_tile");
        let props = prop_names(&schema);
        for expected in ["tab_id", "bounds", "z_order", "ttl_ms"] {
            assert!(
                props.contains(&expected.to_string()),
                "create_tile schema missing property {expected}; has {props:?}"
            );
        }
        // Non-`Option`, non-defaulted fields are required; defaulted/optional are not.
        let req = required(&schema);
        assert!(
            !props.contains(&"namespace".to_string()),
            "namespace is set by the server from identity"
        );
        assert!(req.contains(&"bounds".to_string()));
        assert!(
            !req.contains(&"z_order".to_string()),
            "z_order has a serde default"
        );
        assert!(!req.contains(&"tab_id".to_string()), "tab_id is Option");
        // Nested `bounds` params are inlined (no $ref) with their own required set.
        let bounds = &schema["properties"]["bounds"];
        assert_eq!(bounds["type"], "object");
        let bounds_req = required(bounds);
        for c in ["x", "y", "width", "height"] {
            assert!(
                bounds_req.contains(&c.to_string()),
                "bounds missing required {c}"
            );
        }
    }

    /// Regression for the drift the derive is meant to eliminate: `wait_ms` was
    /// added to `PortalProjectionGetPendingInputParams` and once had to be
    /// hand-mirrored here. It must now appear in `tools/list` purely by virtue
    /// of living on the struct.
    #[test]
    fn wait_ms_field_flows_through_from_struct() {
        let schema = input_schema("portal_projection_get_pending_input");
        assert!(
            prop_names(&schema).contains(&"wait_ms".to_string()),
            "wait_ms must be derived into the inputSchema automatically"
        );
    }

    #[test]
    fn portal_list_schema_has_no_caller_supplied_scope_or_credentials() {
        let schema = input_schema("portal_projection_list");
        assert!(
            prop_names(&schema).is_empty(),
            "list scope is derived from the authenticated resident caller"
        );
        assert!(
            required(&schema).is_empty(),
            "list must not require a projection ID or owner token"
        );
    }

    /// `portal_projection_attach` had grown six fields the hand-written schema
    /// never mirrored. The derive picks them up automatically — proving struct
    /// changes can no longer silently diverge from `tools/list`.
    #[test]
    fn attach_schema_reflects_all_struct_fields() {
        let props = prop_names(&input_schema("portal_projection_attach"));
        for field in [
            "projection_id",
            "display_name",
            "idempotency_key",
            "provider_kind",
            "content_classification",
            "workspace_hint",
            "repository_hint",
            "icon_profile_hint",
            "hud_target",
        ] {
            assert!(
                props.contains(&field.to_string()),
                "attach schema missing struct field {field}; has {props:?}"
            );
        }
    }

    /// hud-jip0k: `expects_reply` (the `Question` signal) must appear in the
    /// `portal_projection_publish` tools/list schema purely by virtue of living
    /// on `PortalProjectionPublishParams` — the same drift-proof property
    /// `wait_ms_field_flows_through_from_struct` locks for the sibling tool.
    #[test]
    fn expects_reply_field_flows_through_from_struct() {
        let schema = input_schema("portal_projection_publish");
        assert!(
            prop_names(&schema).contains(&"expects_reply".to_string()),
            "expects_reply must be derived into the inputSchema automatically"
        );
        assert!(
            !required(&schema).contains(&"expects_reply".to_string()),
            "expects_reply is optional and must not be required"
        );
    }

    /// Mechanism-level proof (independent of any real tool): adding a field to a
    /// `#[derive(JsonSchema)]` params struct surfaces in the derived inputSchema
    /// with no hand-editing. This is the property that makes schema.rs
    /// drift-proof — the real `*Params` structs derive the same way.
    #[test]
    fn added_struct_field_appears_in_derived_schema() {
        #[derive(JsonSchema)]
        #[allow(dead_code)]
        struct FutureParams {
            /// A required field newly added to a params struct.
            newly_added_field: String,
            /// An optional field should not become required.
            #[serde(default)]
            optional_extra: Option<u32>,
        }

        let schema = input_schema_for::<FutureParams>();
        assert_eq!(schema["type"], "object");
        let props = prop_names(&schema);
        assert!(
            props.contains(&"newly_added_field".to_string()),
            "a newly added struct field must flow into the derived schema"
        );
        assert!(props.contains(&"optional_extra".to_string()));
        let req = required(&schema);
        assert!(req.contains(&"newly_added_field".to_string()));
        assert!(
            !req.contains(&"optional_extra".to_string()),
            "an Option/defaulted field must not be marked required"
        );
        // Document-level schemars metadata is stripped from the inputSchema.
        assert!(schema.get("$schema").is_none(), "$schema must be stripped");
        assert!(schema.get("title").is_none(), "title must be stripped");
    }
}
