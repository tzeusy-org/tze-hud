//! Portal visuals resolved from the runtime design-token map.
use tze_hud_config::TimestampGranularity;
use tze_hud_scene::Rgba;

#[derive(Clone, Debug, PartialEq)]
pub struct PortalVisualTokens {
    pub frame_background: Rgba,
    pub frame_opacity: f32,
    pub header_text_color: Rgba,
    pub header_font_size_px: f32,
    pub divider_color: Rgba,
    pub divider_grip_color: Rgba,
    pub content_inset_px: f32,
    pub transcript_background: Rgba,
    pub transcript_text_color: Rgba,
    pub transcript_font_size_px: f32,

    pub transcript_dim_text_color: Rgba,
    pub transcript_dim_background: Rgba,
    pub stale_marker_color: Rgba,
    pub disconnect_badge_color: Rgba,
    pub disconnect_badge_width_px: f32,
    pub unread_indicator_color: Rgba,
    pub unread_divider_color: Rgba,
    pub awaiting_reply_color: Rgba,
    pub empty_state_color: Rgba,
    pub connecting_marker_color: Rgba,
    pub activity_cue_color: Rgba,
    pub streaming_cursor_color: Rgba,

    pub delivery_delivered_color: Rgba,

    pub timestamp_color: Rgba,
    pub timestamp_granularity: TimestampGranularity,

    pub lifecycle_active_color: Rgba,
    pub lifecycle_attached_color: Rgba,
    pub lifecycle_attention_color: Rgba,
    pub lifecycle_inactive_color: Rgba,
    pub lifecycle_accent_width_px: f32,

    pub header_height_px: f32,
    pub section_gap_px: f32,

    pub collapsed_background: Rgba,
    pub collapsed_text_color: Rgba,
    pub collapsed_font_size_px: f32,

    pub composer_background: Rgba,
    pub composer_text_color: Rgba,
    pub composer_font_size_px: f32,
}

impl Default for PortalVisualTokens {
    fn default() -> Self {
        portal_visual_tokens_from_part_tokens(&tze_hud_config::PortalPartTokens::default())
    }
}
pub fn portal_visual_tokens_from_part_tokens(
    part: &tze_hud_config::PortalPartTokens,
) -> PortalVisualTokens {
    let linear_channel = |encoded: f32| {
        if encoded <= 0.04045 {
            encoded / 12.92
        } else {
            ((encoded + 0.055) / 1.055).powf(2.4)
        }
    };
    let linear_color = |color: &tze_hud_config::tokens::Rgba| Rgba {
        r: linear_channel(color.r),
        g: linear_channel(color.g),
        b: linear_channel(color.b),
        a: color.a,
    };
    PortalVisualTokens {
        frame_background: linear_color(&part.frame_background),
        frame_opacity: part.frame_opacity,
        header_text_color: linear_color(&part.header_text_color),
        header_font_size_px: part.header_font_size_px,
        divider_color: linear_color(&part.divider_color),
        divider_grip_color: linear_color(&part.resize_grip_color),
        content_inset_px: part.content_inset_px,
        transcript_background: linear_color(&part.transcript_background),
        transcript_text_color: linear_color(&part.transcript_text_color),
        transcript_font_size_px: part.transcript_font_size_px,
        transcript_dim_text_color: linear_color(&part.transcript_dim_text_color),
        transcript_dim_background: linear_color(&part.transcript_dim_background),
        stale_marker_color: linear_color(&part.stale_marker_color),
        disconnect_badge_color: linear_color(&part.disconnect_badge_color),
        disconnect_badge_width_px: part.disconnect_badge_width_px,
        unread_indicator_color: linear_color(&part.unread_indicator_color),
        unread_divider_color: linear_color(&part.unread_divider_color),
        awaiting_reply_color: linear_color(&part.awaiting_reply_color),
        empty_state_color: linear_color(&part.empty_state_color),
        connecting_marker_color: linear_color(&part.connecting_marker_color),
        activity_cue_color: linear_color(&part.activity_cue_color),
        streaming_cursor_color: linear_color(&part.streaming_cursor_color),
        delivery_delivered_color: linear_color(&part.delivery_delivered_color),
        timestamp_color: linear_color(&part.timestamp_color),
        timestamp_granularity: part.timestamp_granularity,
        lifecycle_active_color: linear_color(&part.lifecycle_active_color),
        lifecycle_attached_color: linear_color(&part.lifecycle_attached_color),
        lifecycle_attention_color: linear_color(&part.lifecycle_attention_color),
        lifecycle_inactive_color: linear_color(&part.lifecycle_inactive_color),
        lifecycle_accent_width_px: part.lifecycle_accent_width_px,
        header_height_px: part.header_height_px,
        section_gap_px: part.section_gap_px,
        collapsed_background: linear_color(&part.collapsed_background),
        collapsed_text_color: linear_color(&part.collapsed_text_color),
        collapsed_font_size_px: part.collapsed_font_size_px,
        composer_background: linear_color(&part.composer_background),
        composer_text_color: linear_color(&part.composer_text_color),
        composer_font_size_px: part.composer_font_size_px,
    }
}
