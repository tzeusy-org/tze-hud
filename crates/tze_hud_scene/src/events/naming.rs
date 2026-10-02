//! # Event Type Naming Convention
//!
//! Dotted event-name grammar, used to validate configured bare event names
//! (`tab_switch_on_event`).
//!
//! ## Naming grammar
//!
//! | Source  | Pattern                                            | Example                              |
//! |---------|----------------------------------------------------|--------------------------------------|
//! | Scene   | `scene.<object>.<action>`                          | `scene.tile.created`                 |
//! | Agent   | `agent.<namespace>.<category>.<action>`            | `agent.doorbell_agent.doorbell.ring` |
//! | System  | `system.<action>`                                  | `system.degradation_changed`         |
//! | Input   | `input.<device>.<action>`                          | `input.pointer.down`                 |
//!
//! ## Segment rules
//!
//! All segments (between dots) must consist only of lowercase ASCII letters,
//! digits, and underscores: `[a-z0-9_]+`.  Each segment must be non-empty and
//! must not start with a digit.
//!
//! ## Reserved prefixes
//!
//! The prefixes `system.` and `scene.` are reserved for runtime-generated
//! events.  Agents **must not** emit events with these prefixes.
//!
//! ## Agent bare names
//!
//! Agents supply a *bare name* (e.g., `doorbell.ring`).  The runtime
//! namespace-prefixes it as `agent.<namespace>.<bare_name>` before delivery.
//! Bare names must match: `[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+`
//! (at least two dot-separated segments, each starting with a letter).

use std::fmt;

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by event type validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamingError {
    /// The event type string is empty.
    Empty,
    /// A segment (between dots) is empty (consecutive dots or leading/trailing dot).
    EmptySegment { position: usize },
    /// A segment contains a character that is not `[a-z0-9_]`.
    InvalidCharacter { segment: String, ch: char },
    /// A segment starts with a digit, which is not allowed.
    SegmentStartsWithDigit { segment: String },
    /// An agent event bare name used a reserved prefix (`system.` or `scene.`).
    ReservedPrefix { prefix: String },
    /// An agent event bare name does not have at least two segments (needs
    /// at least `<category>.<action>`).
    BareTooFewSegments,
}

impl fmt::Display for NamingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamingError::Empty => write!(f, "event type must not be empty"),
            NamingError::EmptySegment { position } => {
                write!(
                    f,
                    "empty segment at position {position} (consecutive dots?)"
                )
            }
            NamingError::InvalidCharacter { segment, ch } => write!(
                f,
                "segment {segment:?} contains invalid character {ch:?} (only [a-z0-9_] allowed)"
            ),
            NamingError::SegmentStartsWithDigit { segment } => {
                write!(f, "segment {segment:?} must not start with a digit")
            }
            NamingError::ReservedPrefix { prefix } => write!(
                f,
                "agent events must not use the reserved prefix {prefix:?}"
            ),
            NamingError::BareTooFewSegments => write!(
                f,
                "agent bare name must have at least two segments (e.g. \"doorbell.ring\")"
            ),
        }
    }
}

impl std::error::Error for NamingError {}

// ─── Segment validation ───────────────────────────────────────────────────────

/// Validate a single dotted-name segment: `[a-z][a-z0-9_]*`.
///
/// Returns `Ok(())` on success or `Err(NamingError)` describing the first
/// problem found.
fn validate_segment(segment: &str, position: usize) -> Result<(), NamingError> {
    if segment.is_empty() {
        return Err(NamingError::EmptySegment { position });
    }
    let first = segment.chars().next().unwrap();
    // First character must be a lowercase letter: [a-z].
    // Digits and underscores are allowed only after the first character.
    if first.is_ascii_digit() {
        return Err(NamingError::SegmentStartsWithDigit {
            segment: segment.to_string(),
        });
    }
    if !first.is_ascii_lowercase() {
        // Catches leading underscores, uppercase letters, and other non-[a-z] starters.
        return Err(NamingError::InvalidCharacter {
            segment: segment.to_string(),
            ch: first,
        });
    }
    for ch in segment.chars() {
        if !matches!(ch, 'a'..='z' | '0'..='9' | '_') {
            return Err(NamingError::InvalidCharacter {
                segment: segment.to_string(),
                ch,
            });
        }
    }
    Ok(())
}

// ─── Agent bare-name validation ───────────────────────────────────────────────

/// Validate an agent-supplied bare event name.
///
/// Bare names are the `<category>.<action>` suffix that agents supply.
/// The runtime prepends `agent.<namespace>.` before delivery.
///
/// Rules (`[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+`):
/// - At least two segments separated by a dot.
/// - Each segment: `[a-z][a-z0-9_]*`.
/// - Must not begin with the reserved prefixes `system.` or `scene.`.
///
/// # Examples
///
/// ```
/// use tze_hud_scene::events::naming::validate_bare_name;
///
/// assert!(validate_bare_name("doorbell.ring").is_ok());
/// assert!(validate_bare_name("fire.detected").is_ok());
/// assert!(validate_bare_name("weather.update").is_ok());
/// assert!(validate_bare_name("system.fake").is_err()); // reserved prefix
/// assert!(validate_bare_name("scene.impersonate").is_err()); // reserved prefix
/// assert!(validate_bare_name("doorbell").is_err()); // needs two segments
/// assert!(validate_bare_name("Doorbell.Ring").is_err()); // uppercase
/// assert!(validate_bare_name("9invalid.start").is_err()); // digit start
/// ```
pub fn validate_bare_name(bare_name: &str) -> Result<(), NamingError> {
    if bare_name.is_empty() {
        return Err(NamingError::Empty);
    }

    // Reserved prefix check.
    if bare_name.starts_with("system.") {
        return Err(NamingError::ReservedPrefix {
            prefix: "system.".to_string(),
        });
    }
    if bare_name.starts_with("scene.") {
        return Err(NamingError::ReservedPrefix {
            prefix: "scene.".to_string(),
        });
    }

    let segments: Vec<&str> = bare_name.split('.').collect();
    if segments.len() < 2 {
        return Err(NamingError::BareTooFewSegments);
    }

    for (i, seg) in segments.iter().enumerate() {
        validate_segment(seg, i)?;
    }

    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_bare_name ────────────────────────────────────────────────────

    /// WHEN an agent attempts to emit an event with name starting with "system."
    /// or "scene." THEN the runtime MUST reject the emission.
    #[test]
    fn reserved_prefix_system_rejected() {
        let err = validate_bare_name("system.fake").unwrap_err();
        assert!(
            matches!(err, NamingError::ReservedPrefix { ref prefix } if prefix == "system."),
            "expected ReservedPrefix(system.), got {err:?}"
        );
    }

    #[test]
    fn reserved_prefix_scene_rejected() {
        let err = validate_bare_name("scene.impersonate").unwrap_err();
        assert!(
            matches!(err, NamingError::ReservedPrefix { ref prefix } if prefix == "scene."),
            "expected ReservedPrefix(scene.), got {err:?}"
        );
    }

    #[test]
    fn valid_bare_names() {
        assert!(validate_bare_name("doorbell.ring").is_ok());
        assert!(validate_bare_name("fire.detected").is_ok());
        assert!(validate_bare_name("weather.update").is_ok());
        assert!(validate_bare_name("status.heartbeat.alive").is_ok());
    }

    #[test]
    fn bare_name_too_few_segments() {
        assert!(matches!(
            validate_bare_name("doorbell"),
            Err(NamingError::BareTooFewSegments)
        ));
    }

    #[test]
    fn bare_name_uppercase_rejected() {
        assert!(validate_bare_name("Doorbell.Ring").is_err());
    }

    #[test]
    fn bare_name_digit_start_rejected() {
        assert!(validate_bare_name("9invalid.start").is_err());
    }

    #[test]
    fn bare_name_leading_dot_rejected() {
        assert!(validate_bare_name(".ring").is_err());
    }

    #[test]
    fn segment_leading_underscore_rejected() {
        // A segment starting with '_' must be rejected — [a-z][a-z0-9_]* requires
        // a lowercase letter as the first character.
        assert!(validate_bare_name("_hidden.event").is_err());
    }
}
