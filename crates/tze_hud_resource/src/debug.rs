//! Operator/debug representation for resource store types.
//!
//! ## Wire vs. debug format
//!
//! - **Wire format**: `ResourceId` is transmitted as raw 32 bytes in a protobuf
//!   `bytes` field.  It MUST NOT be hex-encoded on the wire.
//! - **Operator/debug format**: `ResourceId` is rendered as a 64-character lowercase
//!   hex string for logs, CLI output, and diagnostic tools.  This is the ONLY
//!   acceptable text encoding.
//!
//! The debug/display tests validate that:
//! 1. The `Debug` impl outputs `ResourceId(<hex>)` where `<hex>` is 64 lowercase hex chars.
//! 2. The `Display` impl outputs the 64-character hex string directly.
//! 3. No uppercase hex, no `0x` prefix, no base64 encoding appears.

use crate::types::ResourceId;

// ─── Hex helpers ─────────────────────────────────────────────────────────────

/// Encode a byte slice as a lowercase hex string.
///
/// Helper for rendering `ResourceId` and related types in operator/debug output.
/// Output is always 2 × `bytes.len()` characters, all lowercase.
#[inline]
pub fn to_lowercase_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        write!(s, "{b:02x}").expect("write to String is infallible");
    }
    s
}

/// Return the lowercase hex string for a `ResourceId` (64 chars).
///
/// Equivalent to `id.to_hex()` but provided here as the canonical operator-facing
/// entry point so that the module's purpose is self-documenting.
#[inline]
pub fn resource_id_hex(id: &ResourceId) -> String {
    to_lowercase_hex(id.as_bytes())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Acceptance: Debug/log representation of ResourceId is lowercase hex string
    // (bead rig-6l03 additional acceptance criteria).

    #[test]
    fn resource_id_display_is_64_char_lowercase_hex() {
        // WHEN a ResourceId is formatted with Display
        // THEN the output MUST be exactly 64 lowercase hex characters (no prefix, no spaces).
        let id = ResourceId::from_content(b"display test");
        let s = format!("{id}");
        assert_eq!(s.len(), 64, "Display output must be 64 chars");
        assert!(
            s.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "Display output must be lowercase hex only; got: {s}"
        );
    }

    #[test]
    fn resource_id_debug_contains_lowercase_hex() {
        // WHEN a ResourceId is formatted with Debug
        // THEN the hex portion must use lowercase characters only.
        let id = ResourceId::from_content(b"debug test");
        let s = format!("{id:?}");
        // Debug format is "ResourceId(<hex>)".
        assert!(
            s.starts_with("ResourceId("),
            "Debug output must start with 'ResourceId('; got: {s}"
        );
        // Extract the hex part.
        let inner = s.trim_start_matches("ResourceId(").trim_end_matches(')');
        assert_eq!(
            inner.len(),
            64,
            "hex portion must be 64 chars; got: {inner}"
        );
        assert!(
            inner
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "hex portion must be lowercase only; got: {inner}"
        );
    }

    #[test]
    fn resource_id_hex_no_uppercase() {
        // The spec says lowercase hex.  Ensure no uppercase hex digits appear.
        let id = ResourceId::from_content(b"uppercase check");
        let s = id.to_hex();
        assert!(
            !s.chars().any(|c| c.is_ascii_uppercase()),
            "ResourceId hex MUST NOT contain uppercase letters; got: {s}"
        );
    }

    #[test]
    fn resource_id_hex_no_0x_prefix() {
        // The operator representation is the raw 64-char hex string.
        // No '0x' prefix is permitted.
        let id = ResourceId::from_content(b"no prefix");
        let s = id.to_hex();
        assert!(
            !s.starts_with("0x"),
            "ResourceId hex MUST NOT have '0x' prefix; got: {s}"
        );
    }

    #[test]
    fn resource_id_hex_is_not_base64() {
        // Verify the output is purely lowercase hex, not base64.
        // base64 would include '+', '/', '=', and uppercase chars.
        let id = ResourceId::from_content(b"not base64");
        let s = id.to_hex();
        let invalid_chars: Vec<char> = s
            .chars()
            .filter(|c| !c.is_ascii_digit() && !('a'..='f').contains(c))
            .collect();
        assert!(
            invalid_chars.is_empty(),
            "ResourceId hex contains invalid characters: {invalid_chars:?}; full: {s}"
        );
    }

    #[test]
    fn resource_id_hex_all_zeros() {
        // Edge case: all-zero ResourceId should display as 64 '0' characters.
        let id = ResourceId::from_bytes([0u8; 32]);
        let s = id.to_hex();
        assert_eq!(s, "0".repeat(64), "all-zero ResourceId should be 64 zeros");
    }

    #[test]
    fn resource_id_hex_all_ff() {
        // Edge case: all-0xff ResourceId should display as 64 'f' characters.
        let id = ResourceId::from_bytes([0xffu8; 32]);
        let s = id.to_hex();
        assert_eq!(
            s,
            "f".repeat(64),
            "all-0xff ResourceId should be 64 'f' chars"
        );
    }

    #[test]
    fn to_lowercase_hex_matches_resource_id_to_hex() {
        // The module-level helper must be consistent with ResourceId::to_hex.
        let id = ResourceId::from_content(b"consistency check");
        assert_eq!(
            resource_id_hex(&id),
            id.to_hex(),
            "resource_id_hex helper must match ResourceId::to_hex"
        );
    }

    #[test]
    fn to_lowercase_hex_empty_slice() {
        assert_eq!(to_lowercase_hex(&[]), "");
    }

    #[test]
    fn to_lowercase_hex_single_byte() {
        assert_eq!(to_lowercase_hex(&[0xab]), "ab");
        assert_eq!(to_lowercase_hex(&[0x00]), "00");
        assert_eq!(to_lowercase_hex(&[0xff]), "ff");
    }
}
