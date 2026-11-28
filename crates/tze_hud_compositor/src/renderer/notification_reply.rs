use std::{collections::HashMap, sync::Arc};

use tze_hud_scene::{
    FontFamily, Rect, TextAlign, TextOverflow, graph::overlay::NotificationReplyView,
};

use super::{
    Compositor,
    image_cache::composer_scroll_offset,
    notification_layout::NotificationLayoutCache,
    tile_render::composer_draft_base_color,
    token_colors::{ComposerOverlayTokens, resolve_composer_overlay_tokens, srgb_to_linear},
};
use crate::pipeline::RoundedRectDrawCmd;
use crate::text::{LINE_HEIGHT_MULTIPLIER, StyledRunItem, TextItem};

pub(super) fn reply_style(tokens: &HashMap<String, String>) -> ComposerOverlayTokens {
    let mut style = resolve_composer_overlay_tokens(tokens);
    let scale = tze_hud_config::tokens::px_scale(tokens);
    style.font_size_px *= scale;
    style.content_inset_px *= scale;
    style.caret_width_px *= scale;
    style
}

pub(super) struct ReplyLineGeometry {
    clip: Rect,
    text_x: f32,
    content_width: f32,
    pub(super) caret: Rect,
}

pub(super) fn line_geometry(
    cache: &mut NotificationLayoutCache,
    field: Rect,
    draft: &NotificationReplyView,
    style: &ComposerOverlayTokens,
    tokens: &HashMap<String, String>,
) -> ReplyLineGeometry {
    let inset = style.content_inset_px;
    let line_height = (style.font_size_px * LINE_HEIGHT_MULTIPLIER).max(1.0);
    let clip = Rect::new(
        field.x + inset,
        field.y + inset,
        (field.width - inset * 2.0).max(1.0),
        line_height,
    );
    let layout = cache.reply_line(tokens, &draft.text, style.font_size_px, None);
    let caret_x = layout.x_at_cursor(draft.cursor_byte);
    let content_width = layout.x_at_cursor(draft.text.len());
    let scroll = composer_scroll_offset(caret_x, content_width, clip.width, inset);
    let text_x = clip.x - scroll;
    let caret_width = style.caret_width_px.min(clip.width);
    ReplyLineGeometry {
        clip,
        text_x,
        content_width,
        caret: Rect::new(
            (text_x + caret_x).clamp(clip.x, clip.x + (clip.width - caret_width)),
            clip.y,
            caret_width,
            line_height,
        ),
    }
}

impl ReplyLineGeometry {
    pub(super) fn text_item(
        &self,
        draft: &NotificationReplyView,
        style: &ComposerOverlayTokens,
        opacity: f32,
    ) -> TextItem {
        let start = draft.cursor_byte.min(draft.selection_anchor);
        let end = draft.cursor_byte.max(draft.selection_anchor);
        let styled_runs: Box<[StyledRunItem]> = if start < end {
            Box::new([StyledRunItem {
                start_byte: start,
                end_byte: end,
                weight: None,
                italic: false,
                monospace: false,
                color: None,
                background_color: Some(style.selection_bg),
                size_scale: None,
                fill_line_width: false,
            }])
        } else {
            Box::new([])
        };
        TextItem {
            text: Arc::from(draft.text.as_str()),
            pixel_x: self.text_x,
            pixel_y: self.clip.y,
            bounds_width: self.clip.width.max(self.content_width + style.font_size_px),
            bounds_height: self.clip.height,
            clip_pixel_x: self.clip.x,
            clip_pixel_y: self.clip.y,
            clip_bounds_width: self.clip.width,
            clip_bounds_height: self.clip.height,
            font_size_px: style.font_size_px,
            font_family: FontFamily::SystemSansSerif,
            font_weight: 400,
            color: composer_draft_base_color(style, draft.at_capacity, false),
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            outline_color: None,
            outline_width: None,
            opacity,
            color_runs: Box::new([]),
            styled_runs,
            line_height_multiplier: LINE_HEIGHT_MULTIPLIER,
            viewport: crate::overflow::TruncationViewport::HeadAnchored,
        }
    }
}

impl Compositor {
    pub(super) fn reply_field_commands(
        &self,
        field: Rect,
        caret: Rect,
        style: &ComposerOverlayTokens,
        opacity: f32,
    ) -> [RoundedRectDrawCmd; 2] {
        let [r, g, b, a] = style.caret_color;
        let linear = |v: u8| srgb_to_linear(f32::from(v) / 255.0);
        let command = |rect: Rect, color| RoundedRectDrawCmd {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            color: self.gpu_color_raw(color),
            radius: 0.0,
            border: None,
            clip: None,
        };
        [
            command(
                field,
                [style.bg_r, style.bg_g, style.bg_b, style.bg_a * opacity],
            ),
            command(
                caret,
                [
                    linear(r),
                    linear(g),
                    linear(b),
                    f32::from(a) / 255.0 * opacity,
                ],
            ),
        ]
    }
}
