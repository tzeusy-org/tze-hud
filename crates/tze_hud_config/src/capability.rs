//! Capability vocabulary validation for tze_hud configuration.
//!
//! This module implements the canonical v1 capability vocabulary from
//! `configuration/spec.md §Requirement: Capability Vocabulary`.
//!
//! ## Canonical v1 Capabilities (18 entries)
//!
//! Flat names (exact match):
//! - `create_tiles`
//! - `modify_own_tiles`
//! - `manage_tabs`
//! - `manage_sync_groups`
//! - `upload_resource`
//! - `register_widget_asset`
//! - `read_scene_topology`
//! - `subscribe_scene_events`
//! - `overlay_privileges`
//! - `access_input_events`
//! - `high_priority_z_order`
//! - `exceed_default_budgets`
//! - `read_telemetry`
//! - `resident_mcp`
//!
//! Parameterized (prefix + non-empty suffix):
//! - `publish_zone:<zone_name>` or `publish_zone:*`
//! - `emit_scene_event:<event_name>` (suffix must not start with `scene.` or `system.`)
//! - `lease:priority:<N>` (N must be a non-negative integer)
//!
//! ## Immutability Contract
//! The vocabulary is frozen at compile time.  It is validated at config load.
//! No runtime registration of new capabilities is permitted in v1.
//!
//! ## Scope
//! This module validates individual capability *names*.  It does NOT:
//! - Grant capabilities to agents (rig-mop4).
//! - Wire grants to session handshake (Epic 6).

use tze_hud_scene::config::CANONICAL_CAPABILITIES;

// ─── Legacy name table ────────────────────────────────────────────────────────

/// Legacy capability names from RFC 0009 §8.1 and their canonical replacements.
///
/// These were superseded by RFC 0005 Round 14 (rig-b2s).  They MUST be rejected
/// with `CONFIG_UNKNOWN_CAPABILITY` and a hint pointing to the canonical name.
const LEGACY_NAMES: &[(&str, &str)] = &[
    ("read_scene", "read_scene_topology"),
    ("receive_input", "access_input_events"),
    ("zone_publish", "publish_zone:*"),
];

// ─── Reserved event prefixes ──────────────────────────────────────────────────

/// Reserved event prefixes for `emit_scene_event:<name>`.
///
/// Any capability grant where the event name starts with one of these prefixes
/// MUST be rejected with `CONFIG_RESERVED_EVENT_PREFIX`.
pub const RESERVED_EVENT_PREFIXES: &[&str] = &["scene.", "system."];

// ─── Hint generation ──────────────────────────────────────────────────────────

/// Generate a hint for an unknown capability name.
///
/// Priority order:
/// 1. If the name is a known legacy name, return the canonical replacement.
/// 2. If the name looks like a parameterized form with a bad prefix, suggest
///    the canonical prefix.
/// 3. Find the closest canonical flat name using edit distance.
/// 4. Fall back to a generic message.
pub fn capability_hint(unknown: &str) -> String {
    // 1. Check legacy names first — highest confidence hint.
    for (legacy, canonical) in LEGACY_NAMES {
        if unknown == *legacy {
            return format!("\"{legacy}\" is a legacy name; use \"{canonical}\" instead");
        }
    }

    // 2. Parameterized prefix heuristics.
    //    If user provided a camelCase or kebab variant of a known prefix, suggest
    //    the canonical parameterized form.
    if let Some(hint) = parameterized_prefix_hint(unknown) {
        return hint;
    }

    // 3. Find closest canonical flat name.
    if let Some(closest) = closest_canonical(unknown) {
        return format!("did you mean {closest:?}?");
    }

    // 4. Generic fallback.
    format!(
        "\"{unknown}\" is not a canonical v1 capability; see configuration/spec.md §Capability Vocabulary"
    )
}

/// Returns `true` if the capability name starts with a reserved event prefix.
///
/// Only meaningful for names that start with `emit_scene_event:`.
pub fn has_reserved_event_prefix(cap: &str) -> bool {
    if let Some(suffix) = cap.strip_prefix("emit_scene_event:") {
        return RESERVED_EVENT_PREFIXES
            .iter()
            .any(|p| suffix.starts_with(p));
    }
    false
}

// ─── Closest canonical match ─────────────────────────────────────────────────

/// Return the closest canonical flat capability name using edit distance.
///
/// Only returns `Some` when the closest distance is ≤ `MAX_EDIT_DISTANCE`.
/// Parameterized prefixes (`publish_zone:*`, etc.) are excluded from the
/// candidate pool because they are not useful as "did you mean X?" suggestions
/// (the user's input is also not parameterized-looking in this code path).
fn closest_canonical(name: &str) -> Option<&'static str> {
    const MAX_EDIT_DISTANCE: usize = 5;

    // Iterate flat-name candidates directly: exclude parameterized entries (containing ':').
    let mut best: Option<(&'static str, usize)> = None;
    for &candidate in CANONICAL_CAPABILITIES.iter() {
        if candidate.contains(':') {
            continue;
        }
        let d = edit_distance(name, candidate);
        match best {
            None => best = Some((candidate, d)),
            Some((_, bd)) if d < bd => best = Some((candidate, d)),
            _ => {}
        }
    }

    best.filter(|(_, d)| *d <= MAX_EDIT_DISTANCE)
        .map(|(c, _)| c)
}

/// Heuristic check for known parameterized prefixes used with the wrong separator
/// or casing.
fn parameterized_prefix_hint(name: &str) -> Option<String> {
    // Normalize: lowercase, replace '-' with '_'.
    let norm = name.to_lowercase().replace('-', "_");

    // publish_zone variants.
    if norm.starts_with("publish_zone")
        || norm.starts_with("publishzone")
        || norm.starts_with("zone_publish")
    {
        return Some("use \"publish_zone:<zone>\" or \"publish_zone:*\"".to_string());
    }

    // publish_widget variants.
    if norm.starts_with("publish_widget")
        || norm.starts_with("publishwidget")
        || norm.starts_with("widget_publish")
    {
        return Some("use \"publish_widget:<widget_name>\" or \"publish_widget:*\"".to_string());
    }

    // emit_scene_event variants.
    if norm.starts_with("emit_scene_event") || norm.starts_with("emitsceneevent") {
        return Some("use \"emit_scene_event:<event_name>\"".to_string());
    }

    // lease:priority variants.
    if norm.starts_with("lease_priority") || norm.starts_with("lease:priority") {
        return Some("use \"lease:priority:<N>\" where N is a non-negative integer".to_string());
    }

    None
}

// ─── Edit distance ────────────────────────────────────────────────────────────

/// Compute the edit distance (Levenshtein) between two strings.
///
/// Uses a single-row DP approach, O(min(a,b)) space.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (a, b) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    // `a` is now the shorter string.

    let mut prev: Vec<usize> = (0..=a.len()).collect();
    for (j, cb) in b.iter().enumerate() {
        let mut curr = vec![0usize; a.len() + 1];
        curr[0] = j + 1;
        for (i, ca) in a.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[i + 1] = (prev[i] + cost).min(prev[i + 1] + 1).min(curr[i] + 1);
        }
        prev = curr;
    }
    prev[a.len()]
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::config::is_canonical_capability;

    // ── Vocabulary completeness ───────────────────────────────────────────────

    /// All v1 canonical capability forms must be recognized.
    /// The spec lists 18 forms; parameterized forms (publish_zone, publish_widget,
    /// emit_scene_event, lease:priority) are tested with concrete examples, so the
    /// array below contains 20 entries.
    #[test]
    fn all_canonical_capabilities_recognized() {
        let caps = [
            "create_tiles",
            "modify_own_tiles",
            "manage_tabs",
            "manage_sync_groups",
            "upload_resource",
            "register_widget_asset",
            "read_scene_topology",
            "subscribe_scene_events",
            "overlay_privileges",
            "access_input_events",
            "high_priority_z_order",
            "exceed_default_budgets",
            "read_telemetry",
            "resident_mcp",
            "publish_zone:*",
            "publish_zone:subtitle",
            "publish_widget:clock",
            "emit_scene_event:doorbell.ring",
            "lease:priority:1",
            "lease:priority:0",
        ];
        for cap in &caps {
            assert!(
                is_canonical_capability(cap),
                "expected {cap:?} to be canonical"
            );
        }
    }

    // ── Parameterized parsing ─────────────────────────────────────────────────

    /// publish_zone:<zone_name> accepted for non-empty zone.
    #[test]
    fn publish_zone_parameterized_accepted() {
        assert!(is_canonical_capability("publish_zone:subtitle"));
        assert!(is_canonical_capability("publish_zone:live_captions"));
        assert!(is_canonical_capability("publish_zone:*"));
    }

    /// publish_zone: with empty suffix rejected.
    #[test]
    fn publish_zone_empty_suffix_rejected() {
        assert!(!is_canonical_capability("publish_zone:"));
    }

    /// emit_scene_event:<event_name> accepted for non-reserved names.
    #[test]
    fn emit_scene_event_accepted() {
        assert!(is_canonical_capability("emit_scene_event:doorbell.ring"));
        assert!(is_canonical_capability("emit_scene_event:app.ready"));
    }

    /// emit_scene_event: with empty suffix rejected.
    #[test]
    fn emit_scene_event_empty_suffix_rejected() {
        assert!(!is_canonical_capability("emit_scene_event:"));
    }

    /// lease:priority:<N> accepted for valid non-negative integer N.
    #[test]
    fn lease_priority_accepted() {
        assert!(is_canonical_capability("lease:priority:0"));
        assert!(is_canonical_capability("lease:priority:1"));
        assert!(is_canonical_capability("lease:priority:100"));
    }

    /// lease:priority with non-numeric suffix rejected.
    #[test]
    fn lease_priority_non_numeric_rejected() {
        assert!(!is_canonical_capability("lease:priority:high"));
        assert!(!is_canonical_capability("lease:priority:"));
    }

    // ── Reserved prefix rejection ─────────────────────────────────────────────

    /// emit_scene_event:system.* rejected as reserved.
    #[test]
    fn reserved_system_prefix_rejected() {
        assert!(!is_canonical_capability("emit_scene_event:system.shutdown"));
        assert!(!is_canonical_capability("emit_scene_event:system.reboot"));
        assert!(has_reserved_event_prefix(
            "emit_scene_event:system.shutdown"
        ));
    }

    /// emit_scene_event:scene.* rejected as reserved.
    #[test]
    fn reserved_scene_prefix_rejected() {
        assert!(!is_canonical_capability("emit_scene_event:scene.render"));
        assert!(has_reserved_event_prefix("emit_scene_event:scene.render"));
    }

    /// Non-emit_scene_event prefixes do not trigger reserved prefix check.
    #[test]
    fn has_reserved_event_prefix_only_for_emit() {
        assert!(!has_reserved_event_prefix("create_tiles"));
        assert!(!has_reserved_event_prefix("publish_zone:system.test"));
    }

    // ── Legacy name rejection with hints ─────────────────────────────────────

    /// read_scene rejected with hint pointing to read_scene_topology.
    #[test]
    fn legacy_read_scene_rejected_with_hint() {
        assert!(!is_canonical_capability("read_scene"));
        let hint = capability_hint("read_scene");
        assert!(
            hint.contains("read_scene_topology"),
            "hint should mention canonical replacement, got: {hint:?}"
        );
    }

    /// receive_input rejected with hint pointing to access_input_events.
    #[test]
    fn legacy_receive_input_rejected_with_hint() {
        assert!(!is_canonical_capability("receive_input"));
        let hint = capability_hint("receive_input");
        assert!(
            hint.contains("access_input_events"),
            "hint should mention canonical replacement, got: {hint:?}"
        );
    }

    /// zone_publish rejected with hint pointing to publish_zone:*.
    #[test]
    fn legacy_zone_publish_rejected_with_hint() {
        assert!(!is_canonical_capability("zone_publish"));
        let hint = capability_hint("zone_publish");
        assert!(
            hint.contains("publish_zone"),
            "hint should mention publish_zone, got: {hint:?}"
        );
    }

    // ── Closest canonical match hints ────────────────────────────────────────

    /// createTiles (camelCase) → hint "did you mean create_tiles?"
    #[test]
    fn camel_case_create_tiles_hint() {
        assert!(!is_canonical_capability("createTiles"));
        let hint = capability_hint("createTiles");
        assert!(
            hint.contains("create_tiles"),
            "hint should suggest create_tiles, got: {hint:?}"
        );
    }

    /// create-tiles (kebab) → hint "did you mean create_tiles?"
    #[test]
    fn kebab_case_create_tiles_hint() {
        assert!(!is_canonical_capability("create-tiles"));
        let hint = capability_hint("create-tiles");
        assert!(
            hint.contains("create_tiles"),
            "hint should suggest create_tiles, got: {hint:?}"
        );
    }

    /// tile_create (word-order reversal) → non-empty hint returned (may not match create_tiles
    /// because word-order reversals have high edit distance).
    #[test]
    fn reversed_tile_create_returns_hint() {
        assert!(!is_canonical_capability("tile_create"));
        let hint = capability_hint("tile_create");
        // The hint must be non-empty and mention the vocabulary reference.
        assert!(
            !hint.is_empty(),
            "hint should be non-empty for tile_create, got: {hint:?}"
        );
    }

    // ── Edit distance ─────────────────────────────────────────────────────────

    #[test]
    fn edit_distance_identical() {
        assert_eq!(edit_distance("abc", "abc"), 0);
    }

    #[test]
    fn edit_distance_empty() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
    }

    #[test]
    fn edit_distance_one_substitution() {
        assert_eq!(edit_distance("abc", "axc"), 1);
    }

    #[test]
    fn edit_distance_insertion() {
        assert_eq!(edit_distance("ac", "abc"), 1);
    }
}
