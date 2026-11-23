//! Notification icon parsing helper.

use tze_hud_scene::types::*;

// ─── Notification icon helpers ───────────────────────────────────────────────

/// Parse a `NotificationPayload.icon` string as a hex-encoded `ResourceId`.
///
/// Returns `Some(resource_id)` only when:
/// - The icon string is non-empty.
/// - It is exactly 64 hex characters (the `ResourceId::to_hex()` format).
///
/// Returns `None` for empty strings, human-readable names (e.g. `"shield"`),
/// or malformed hex. Callers MUST check the image_texture_cache before emitting
/// a draw command — this function does not verify that the texture is loaded.
#[inline]
pub(super) fn parse_notification_icon(icon: &str) -> Option<ResourceId> {
    if icon.is_empty() {
        return None;
    }
    ResourceId::from_hex(icon)
}
