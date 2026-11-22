//! Notification card text geometry, shared by drawing and admission-time
//! fit measurement ([`super::text_fit`]) so the two cannot drift.

use std::collections::HashMap;

use tze_hud_scene::types::{FontFamily, NotificationPayload, RenderingPolicy};

use super::notification_style::NotificationStyle;

/// One ellipsized text line band of a card.
pub(super) struct NotificationTextBand<'a> {
    /// The publish field this band draws (`"title"` or `"body"`), or
    /// `"caption"`: the session label, which no publish field carries.
    pub(super) field: &'static str,
    pub(super) text: &'a str,
    /// Offset from the slot top.
    pub(super) y: f32,
    pub(super) height: f32,
    pub(super) font_size_px: f32,
    pub(super) font_weight: u16,
    /// `TextItem::line_height_multiplier` (None: the renderer default).
    pub(super) line_multiplier: Option<f32>,
}

/// The card's text column: every band shares its x offset and width.
pub(super) struct NotificationTextLayout<'a> {
    /// Offset from the zone's left edge.
    pub(super) x: f32,
    pub(super) width: f32,
    pub(super) font_family: FontFamily,
    pub(super) bands: Vec<NotificationTextBand<'a>>,
}

/// Lay out a card's (or alert banner's) title/body bands. `zone_tokens` is
/// the zone's monitor-scaled token map; `agent_icon` says whether the
/// publication's own icon is drawn. The icon column is reserved for it or for
/// the banner's severity mark. Bands are ellipsized one-line text. A
/// `caption` (the publishing session's label) takes one secondary line above
/// the title and moves the rest down unchanged.
pub(super) fn notification_text_layout<'a>(
    zone_name: &str,
    zone_tokens: &HashMap<String, String>,
    policy: &RenderingPolicy,
    payload: &'a NotificationPayload,
    caption: Option<&'a str>,
    card_w: f32,
    agent_icon: bool,
) -> NotificationTextLayout<'a> {
    let style = NotificationStyle::for_zone(zone_name, zone_tokens, policy);
    let icon_shown = agent_icon || style.mark(payload.urgency, agent_icon).is_some();
    let left = style.text_left(icon_shown);
    // The × sits in the title row at the right padding.
    let width = (card_w - left - style.padding_h - style.dismiss_size - style.dismiss_gap).max(1.0);
    let band = |field, text, y, role: super::notification_style::TextRole| NotificationTextBand {
        field,
        text,
        y,
        height: role.line_height.max(1.0),
        font_size_px: role.size,
        font_weight: role.weight,
        line_multiplier: Some(role.multiplier()),
    };
    let mut bands: Vec<_> = caption
        .map(|label| band("caption", label, style.padding_v, style.body))
        .into_iter()
        .collect();
    let top = style.padding_v + style.caption_height(caption);
    bands.extend(if payload.title.is_empty() {
        // A lone line is the message: the body role on a card, the title
        // role on an alert banner.
        vec![band("body", &payload.text, top, style.lone)]
    } else {
        vec![
            band("title", &payload.title, top, style.title),
            band(
                "body",
                &payload.text,
                top + style.title.line_height,
                style.body,
            ),
        ]
    });
    NotificationTextLayout {
        x: left,
        width,
        font_family: policy.font_family.unwrap_or(FontFamily::SystemSansSerif),
        bands,
    }
}
