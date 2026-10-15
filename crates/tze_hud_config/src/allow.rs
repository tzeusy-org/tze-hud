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
//! - `admin` — operator endpoints (`/admin/*`). Opt-in: `*` does not grant it.
//! - `*` — everything else.
//!
//! Inside the runtime, allow entries expand to the scene's internal
//! permission strings via [`allow_to_permissions`]; nothing outside the
//! runtime sees those strings.

/// Validate one allow entry. Returns a hint for an unknown entry.
pub fn validate_allow_entry(entry: &str) -> Result<(), String> {
    match entry {
        "*" | "portal" | "tiles" | "admin" => Ok(()),
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
                 \"widget:<name|*>\", \"portal\", \"tiles\", \"admin\", \"*\""
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
            "admin" => push(tze_hud_scene::config::OPERATOR_ADMIN),
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

/// A surface class granted or withdrawn as a whole (the control panel's
/// allow-list toggles).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    Zones,
    Widgets,
    Portal,
    Tiles,
    Admin,
}

impl Grant {
    pub const ALL: [Grant; 5] = [
        Grant::Zones,
        Grant::Widgets,
        Grant::Portal,
        Grant::Tiles,
        Grant::Admin,
    ];

    /// The allow entry that grants the whole class.
    pub fn entry(self) -> &'static str {
        match self {
            Grant::Zones => "zone:*",
            Grant::Widgets => "widget:*",
            Grant::Portal => "portal",
            Grant::Tiles => "tiles",
            Grant::Admin => "admin",
        }
    }

    /// Whether `allow` grants the whole class (`*` covers all but admin).
    pub fn granted(self, allow: &[String]) -> bool {
        allow
            .iter()
            .any(|e| e == self.entry() || (e == "*" && self != Grant::Admin))
    }
}

/// `allow` with `grant` on or off. Withdrawing a class that `*` covered
/// rewrites `*` as the classes that remain. Entries naming one zone or widget
/// are kept either way.
pub fn with_grant(allow: &[String], grant: Grant, on: bool) -> Vec<String> {
    if grant.granted(allow) == on {
        return allow.to_vec();
    }
    let mut out: Vec<String> = Vec::with_capacity(allow.len() + 3);
    for entry in allow {
        if entry == "*" && !on {
            // Withdrawing a class `*` covered.
            out.extend(
                Grant::ALL
                    .into_iter()
                    .filter(|g| *g != grant && *g != Grant::Admin)
                    .map(|g| g.entry().to_owned()),
            );
        } else if entry != grant.entry() {
            out.push(entry.clone());
        }
    }
    if on {
        out.push(grant.entry().to_owned());
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|e| seen.insert(e.clone()));
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
            "admin",
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
        // `admin` is its own permission, never implied by `*`.
        let admin = allow_to_permissions(&["*".into(), "admin".into()]);
        assert_eq!(admin, vec!["*".to_string(), "operator_admin".to_string()]);
    }

    /// Withdrawing a class `*` covered spells out the rest; granting adds the
    /// class entry; single-surface entries survive both (hud-vniri.6).
    #[test]
    fn grants_toggle_whole_classes_and_expand_star() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let star = v(&["*", "admin"]);
        assert!(Grant::Zones.granted(&star) && Grant::Admin.granted(&star));
        assert!(!Grant::Admin.granted(&v(&["*"])), "* never grants admin");

        let no_zones = with_grant(&star, Grant::Zones, false);
        assert_eq!(no_zones, v(&["widget:*", "portal", "tiles", "admin"]));
        assert!(!Grant::Zones.granted(&no_zones));
        assert!(Grant::Tiles.granted(&no_zones));
        assert_eq!(
            with_grant(&no_zones, Grant::Zones, true),
            v(&["widget:*", "portal", "tiles", "admin", "zone:*"])
        );

        let narrow = v(&["zone:subtitle", "tiles"]);
        assert_eq!(
            with_grant(&narrow, Grant::Tiles, false),
            v(&["zone:subtitle"])
        );
        assert_eq!(
            with_grant(&narrow, Grant::Zones, true),
            v(&["zone:subtitle", "tiles", "zone:*"])
        );
        assert_eq!(with_grant(&narrow, Grant::Portal, false), narrow, "no-op");
        assert_eq!(
            with_grant(&v(&["*"]), Grant::Admin, true),
            star,
            "granting admin keeps `*`"
        );
        for g in Grant::ALL {
            assert!(validate_allow_entry(g.entry()).is_ok());
        }
    }
}
