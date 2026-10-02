//! MCP lifecycle and introspection payloads (`initialize`, `tools/list`).
//!
//! The five tool schemas are written by hand: they are the product's model
//! surface, and every byte is paid in each session's context. The
//! `tools_list_*` tests below pin them to the `*Params` structs in
//! [`crate::tools`] (`deny_unknown_fields`) and to the token budget.

use serde_json::{Value, json};

/// MCP protocol revision advertised by `initialize`.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Build the `initialize` handshake result.
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "serverInfo": {
            "name": "tze_hud",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "instructions": "Call hud_surfaces, then hud_publish to a surface it lists.",
    })
}

/// Build the `tools/list` result.
pub fn tools_list_result() -> Value {
    let surface = json!({"type": "string", "description": "From hud_surfaces, e.g. zone:subtitle"});
    let ttl = json!({"type": "integer", "minimum": 0, "description": "0 = until cleared"});
    json!({ "tools": [
        {
            "name": "hud_surfaces",
            "description": "List HUD surfaces you may use and what you hold.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "hud_publish",
            "description": "Show content on a surface (replaces your previous content there). \
                Zone: content is text, or an object for structured zones. Widget: params. \
                Portal: first publish attaches; content is output text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "surface": surface,
                    "content": {"type": ["string", "object"]},
                    "params": {"type": "object"},
                    "ttl_ms": {"type": "integer", "minimum": 0, "description": "zone default 60000; 0 = until cleared"},
                    "delay_ms": {"type": "integer", "minimum": 0, "maximum": 300000, "description": "show later"},
                    "key": {"type": "string", "description": "merge key: same key replaces"},
                    "status": {"type": "string", "enum": ["attached", "active", "degraded", "hud_unavailable", "detached"]},
                    "expects_reply": {"type": "boolean"},
                    "display_name": {"type": "string"},
                },
                "required": ["surface"],
            },
        },
        {
            "name": "hud_hold",
            "description": "Keep your content on a surface without resending it.",
            "inputSchema": {
                "type": "object",
                "properties": {"surface": surface, "ttl_ms": ttl},
                "required": ["surface", "ttl_ms"],
            },
        },
        {
            "name": "hud_clear",
            "description": "Remove your content from a surface, or detach a portal.",
            "inputSchema": {
                "type": "object",
                "properties": {"surface": surface, "reason": {"type": "string"}},
                "required": ["surface"],
            },
        },
        {
            "name": "hud_input",
            "description": "Get user input (portal replies, notification actions). \
                Pass ids in ack once handled; unacked items repeat.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ack": {"type": "array", "items": {"type": "string"}},
                    "wait_ms": {"type": "integer", "minimum": 0, "maximum": 30000},
                    "max_items": {"type": "integer", "minimum": 1},
                },
            },
        },
    ]})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ClearParams, HoldParams, InputParams, PublishParams};
    use serde::de::DeserializeOwned;

    /// Rough o200k estimate (bytes / 4 overestimates JSON slightly); the exact
    /// count is gated by `scripts/ci/check_token_footprint.py`.
    #[test]
    fn tools_list_stays_within_token_budget() {
        let bytes = serde_json::to_string(&tools_list_result()).unwrap().len();
        assert!(
            bytes <= 3_600,
            "tools/list is {bytes} bytes (~{} tokens); budget is 900 tokens",
            bytes / 4
        );
    }

    fn props(tool: &str) -> Vec<String> {
        let list = tools_list_result();
        let t = list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == tool)
            .unwrap()
            .clone();
        t["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    /// Every advertised property must deserialize into the params struct
    /// (they deny unknown fields), so the schema cannot drift ahead of the code.
    fn assert_props_accepted<T: DeserializeOwned>(
        tool: &str,
        base: Value,
        sample: impl Fn(&str) -> Value,
    ) {
        for p in props(tool) {
            let mut args = base.clone();
            args[p.as_str()] = sample(&p);
            serde_json::from_value::<T>(args).unwrap_or_else(|e| panic!("{tool}.{p}: {e}"));
        }
    }

    #[test]
    fn tools_list_properties_match_params_structs() {
        let sample = |p: &str| match p {
            "content" | "surface" | "key" | "status" | "reason" | "display_name" => json!("x"),
            "params" => json!({}),
            "expects_reply" => json!(true),
            "ack" => json!(["i1"]),
            _ => json!(1),
        };
        assert_props_accepted::<PublishParams>("hud_publish", json!({"surface": "zone:x"}), sample);
        assert_props_accepted::<HoldParams>(
            "hud_hold",
            json!({"surface": "zone:x", "ttl_ms": 1}),
            sample,
        );
        assert_props_accepted::<ClearParams>("hud_clear", json!({"surface": "zone:x"}), sample);
        for p in props("hud_input") {
            let args = json!({ p.as_str(): sample(&p) });
            serde_json::from_value::<InputParams>(args).unwrap();
        }
        assert!(props("hud_surfaces").is_empty());
    }

    #[test]
    fn exactly_five_tools() {
        let names: Vec<_> = tools_list_result()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "hud_surfaces",
                "hud_publish",
                "hud_hold",
                "hud_clear",
                "hud_input"
            ]
        );
    }
}
