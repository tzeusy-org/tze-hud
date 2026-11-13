//! Notification card text geometry, shared by drawing and admission-time
//! fit measurement ([`super::text_fit`]) so the two cannot drift.

use std::collections::HashMap;

use tze_hud_scene::types::{FontFamily, NotificationPayload, RenderingPolicy};

use super::Compositor;
use super::notification_layout::NotificationActionLayout;
use super::token_colors::{
    NOTIFICATION_BODY_SCALE, NOTIFICATION_DISMISS_BUTTON_SIZE_PX, NOTIFICATION_DISMISS_GAP_PX,
    NOTIFICATION_ICON_GAP_PX, NOTIFICATION_ICON_SIZE_PX, NOTIFICATION_INSET_DEFAULT_PX,
    NOTIFICATION_INTER_LINE_GAP, NOTIFICATION_TITLE_WEIGHT, is_alert_banner_zone,
    notification_inset,
};

/// One ellipsized text line band of a card.
pub(super) struct NotificationTextBand<'a> {
    /// The publish field this band draws (`"title"` or `"body"`).
    pub(super) field: &'static str,
    pub(super) text: &'a str,
    /// Offset from the slot top.
    pub(super) y: f32,
    pub(super) height: f32,
    pub(super) font_size_px: f32,
    pub(super) font_weight: u16,
}

/// The card's text column: every band shares its x offset and width.
pub(super) struct NotificationTextLayout<'a> {
    /// Offset from the zone's left edge.
    pub(super) x: f32,
    pub(super) width: f32,
    pub(super) font_family: FontFamily,
    pub(super) bands: Vec<NotificationTextBand<'a>>,
}

/// Lay out a card's title/body bands. `global_tokens` and `zone_tokens` are
/// the compositor's token map and the zone's monitor-scaled copy; `icon_shown`
/// reserves the icon column. Bands are ellipsized one-line text (body may
/// wrap only within its band height).
#[allow(clippy::too_many_arguments)]
pub(super) fn notification_text_layout<'a>(
    zone_name: &str,
    global_tokens: &HashMap<String, String>,
    zone_tokens: &HashMap<String, String>,
    policy: &RenderingPolicy,
    payload: &'a NotificationPayload,
    zw: f32,
    effective_slot_h: f32,
    actions: &NotificationActionLayout,
    icon_shown: bool,
    scale: f32,
) -> NotificationTextLayout<'a> {
    let inset_h = notification_inset(policy, scale);
    let inset_v = policy
        .margin_vertical
        .or(policy.margin_px)
        .unwrap_or(NOTIFICATION_INSET_DEFAULT_PX * scale);
    let icon_width_reserved = if icon_shown {
        (NOTIFICATION_ICON_SIZE_PX + NOTIFICATION_ICON_GAP_PX) * scale
    } else {
        0.0
    };
    // Font size: policy explicit > typography.body.size token > 16px
    let font_size_px = policy
        .font_size_px
        .unwrap_or_else(|| Compositor::resolve_body_font_size(zone_tokens));
    let font_family = policy.font_family.unwrap_or(FontFamily::SystemSansSerif);
    // The × sits inset_h in from the right edge, as the text does: reserve
    // its width and gap. Alert banners have no ×.
    let dismiss_reserved_w = if is_alert_banner_zone(zone_name) {
        0.0
    } else {
        (NOTIFICATION_DISMISS_BUTTON_SIZE_PX + NOTIFICATION_DISMISS_GAP_PX) * scale
    };
    let width = (zw - inset_h - icon_width_reserved - inset_h - dismiss_reserved_w).max(1.0);
    // Text stops above the action row (hud-mskx5).
    let text_slot_h = (effective_slot_h - actions.reserve(inset_h)).max(1.0);
    let bands = if payload.title.is_empty() {
        // Single-line rendering (backward-compatible).
        vec![NotificationTextBand {
            field: "body",
            text: &payload.text,
            y: inset_v,
            height: (text_slot_h - inset_v * 2.0).max(1.0),
            font_size_px,
            font_weight: policy.font_weight.unwrap_or(400),
        }]
    } else {
        // Two-line rendering: bold title + regular body, with the same
        // resolved typography as the slot-height calculation.
        let body_scale = global_tokens
            .get("typography.notification.body.scale")
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(NOTIFICATION_BODY_SCALE)
            .clamp(0.5, 1.0);
        let title_weight = global_tokens
            .get("typography.notification.title.weight")
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(NOTIFICATION_TITLE_WEIGHT);
        let title_line_h = font_size_px * 1.4;
        let body_y = inset_v + title_line_h + NOTIFICATION_INTER_LINE_GAP * scale;
        vec![
            NotificationTextBand {
                field: "title",
                text: &payload.title,
                y: inset_v,
                height: title_line_h.max(1.0),
                font_size_px,
                font_weight: title_weight,
            },
            NotificationTextBand {
                field: "body",
                text: &payload.text,
                y: body_y,
                // Remaining slot height available for body (down to inset bottom).
                height: ((text_slot_h - inset_v) - body_y).max(1.0),
                font_size_px: font_size_px * body_scale,
                font_weight: 400,
            },
        ]
    };
    NotificationTextLayout {
        x: inset_h + icon_width_reserved,
        width,
        font_family,
        bands,
    }
}
