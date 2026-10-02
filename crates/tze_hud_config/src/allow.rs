//! Per-agent allow lists (`[agents.<id>] allow = [...]` in `agents.toml`).
//!
//! The allow list is the whole permission model: each entry names a surface
//! class the agent may use. Entries:
//!
//! - `zone:<name>` or `zone:*` — publish to (and clear) a zone.
//! - `widget:<name>` or `widget:*` — publish to (and clear) a widget instance.
//! - `portal` — session-portal projection tools.
//! - `tiles` — resident tiles: lease, create, mutate, tabs, image upload,
//!   input/focus subscriptions.
//! - `*` — everything.
//!
//! Inside the runtime, allow entries expand to the scene's internal
//! permission strings via [`allow_to_permissions`]; nothing outside the
//! runtime sees those strings.

/// Validate one allow entry. Returns a hint for an unknown entry.
pub fn validate_allow_entry(entry: &str) -> Result<(), String> {
    match entry {
        "*" | "portal" | "tiles" => Ok(()),
        _ => {
            if let Some(name) = entry
                .strip_prefix("zone:")
                .or_else(|| entry.strip_prefix("widget:"))
            {
                if name.is_empty() {
                    return Err(format!(
                        "{entry:?} is missing a name; use e.g. \"zone:subtitle\" or \"zone:*\""
                    ));
                }
                return Ok(());
            }
            Err(format!(
                "unknown allow entry {entry:?}; valid entries: \"zone:<name|*>\", \
                 \"widget:<name|*>\", \"portal\", \"tiles\", \"*\""
            ))
        }
    }
}

/// Permissions granted by the `tiles` entry.
const TILE_PERMISSIONS: &[&str] = &[
    "create_tiles",
    "modify_own_tiles",
    "manage_tabs",
    "upload_resource",
    "register_widget_asset",
    "read_scene_topology",
    "access_input_events",
    "read_telemetry",
];

/// Expand an allow list into the runtime's internal permission strings.
///
/// `*` expands to the unrestricted marker `"*"`.
pub fn allow_to_permissions(allow: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: &str| {
        if !out.iter().any(|e| e == p) {
            out.push(p.to_string());
        }
    };
    for entry in allow {
        match entry.as_str() {
            "*" => push("*"),
            "tiles" => TILE_PERMISSIONS.iter().for_each(|p| push(p)),
            "portal" => push("resident_mcp"),
            other => {
                if let Some(zone) = other.strip_prefix("zone:") {
                    push(&format!("publish_zone:{zone}"));
                } else if let Some(widget) = other.strip_prefix("widget:") {
                    push(&format!("publish_widget:{widget}"));
                    // Widget agents may register SVG assets for their widgets.
                    push("register_widget_asset");
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_entries_pass() {
        for e in [
            "*",
            "portal",
            "tiles",
            "zone:subtitle",
            "zone:*",
            "widget:gauge",
        ] {
            assert!(validate_allow_entry(e).is_ok(), "{e}");
        }
    }

    #[test]
    fn unknown_entries_carry_a_hint() {
        let hint = validate_allow_entry("create_tiles").unwrap_err();
        assert!(hint.contains("valid entries"), "{hint}");
        assert!(
            validate_allow_entry("zone:")
                .unwrap_err()
                .contains("missing a name")
        );
    }

    #[test]
    fn expansion_maps_surfaces_to_permissions() {
        let p = allow_to_permissions(&["zone:subtitle".into(), "portal".into(), "tiles".into()]);
        assert!(p.contains(&"publish_zone:subtitle".to_string()));
        assert!(p.contains(&"resident_mcp".to_string()));
        assert!(p.contains(&"create_tiles".to_string()));
        assert_eq!(allow_to_permissions(&["*".into()]), vec!["*".to_string()]);
    }
}
