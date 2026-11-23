//! The full text of a painted ellipsized notification field.

use std::sync::Arc;

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::graph::overlay::{
    NotificationRevealField, NotificationRevealPanel, NotificationRevealRegion,
    NotificationRevealTarget,
};
use tze_hud_scene::types::{FontFamily, Rect, TextAlign, TextOverflow, ZonePublishRecord};

use super::Compositor;
use super::notification_style::{color, dp};
use crate::pipeline::{RoundedRectBorder, RoundedRectDrawCmd};
use crate::text::{self, TextItem};

pub(super) struct RevealCandidate {
    pub(super) region: NotificationRevealRegion,
    card: Rect,
    token_key: String,
    text: Arc<str>,
    font: FontFamily,
}

impl RevealCandidate {
    pub(super) fn new(
        record: &ZonePublishRecord,
        card: Rect,
        token_key: &str,
        field: NotificationRevealField,
        item: &TextItem,
        bounds: Rect,
    ) -> Self {
        let id = record.publication_id;
        let focus_id = match field {
            NotificationRevealField::Title => format!("notification-reveal:{id}:title"),
            NotificationRevealField::Body => format!("notification-reveal:{id}:body"),
            NotificationRevealField::Action(index) => {
                super::hit_regions::notification_action_interaction_id(
                    &record.zone_name,
                    record,
                    index,
                )
            }
        };
        Self {
            region: NotificationRevealRegion {
                target: NotificationRevealTarget {
                    publication_id: id,
                    field,
                },
                bounds,
                focus_id,
            },
            card,
            token_key: token_key.to_owned(),
            text: Arc::clone(&item.text),
            font: item.font_family,
        }
    }
}

#[derive(Default)]
pub(super) struct RevealFrame {
    pub(super) scene: Option<(u64, u64)>,
    pub(super) regions: Vec<NotificationRevealRegion>,
    pub(super) panel: Option<NotificationRevealPanel>,
}

pub(super) struct RevealMeasure {
    target: NotificationRevealTarget,
    width: f32,
    text_size: f32,
    height: f32,
}

/// Prefer a place outside the whole card. If the card fills the display,
/// place the panel outside the source field so the hovered control stays clear.
fn placement(
    display: Rect,
    card: Rect,
    source: Rect,
    size: (f32, f32),
    gap: f32,
    min_h: f32,
) -> Option<Rect> {
    let (width, height) = size;
    let x = card
        .x
        .clamp(display.x, display.x + (display.width - width).max(0.0));
    let edges = |anchor: Rect| {
        let bottom = (anchor.y + anchor.height + gap).max(display.y);
        let above = (anchor.y - gap).min(display.y + display.height);
        [
            (bottom, display.y + display.height - bottom, false),
            (above, above - display.y, true),
        ]
    };
    let (edge, room, above) = edges(card)
        .into_iter()
        .find(|&(_, room, _)| room >= height)
        .or_else(|| {
            edges(card)
                .into_iter()
                .filter(|&(_, room, _)| room >= min_h)
                .max_by(|a, b| a.1.total_cmp(&b.1))
        })
        .or_else(|| {
            edges(source)
                .into_iter()
                .filter(|&(_, room, _)| room >= min_h)
                .max_by(|a, b| a.1.total_cmp(&b.1))
        })?;
    let height = height.min(room);
    Some(Rect::new(
        x,
        if above { edge - height } else { edge },
        width,
        height,
    ))
}

fn pointer_bridge(source: Rect, panel: Rect) -> Rect {
    let left = source.x.max(panel.x);
    let right = (source.x + source.width).min(panel.x + panel.width);
    let (top, bottom) = if panel.y >= source.y {
        (source.y + source.height, panel.y)
    } else {
        (panel.y + panel.height, source.y)
    };
    Rect::new(left, top, (right - left).max(0.0), (bottom - top).max(0.0))
}

impl Compositor {
    pub(super) fn notification_text_was_ellipsized(&self, item: &TextItem) -> bool {
        let Some(rasterizer) = &self.text_rasterizer else {
            return false;
        };
        let Some(key) = text::effective_truncation_key(item) else {
            return false;
        };
        rasterizer
            .truncation_cache
            .get_by_key(&key)
            .or_else(|| {
                rasterizer
                    .truncation_cache
                    .get_stale_by_content(&key.content_hash())
            })
            .is_some_and(|result| result.was_truncated)
    }

    pub(super) fn publish_notification_reveal_geometry(&self, scene: &mut SceneGraph) {
        let frame = &self.notification_reveal_frame;
        if frame.scene == Some((scene.instance_id, scene.version)) {
            scene
                .overlay
                .notification_reveal_regions
                .clone_from(&frame.regions);
            scene.overlay.notification_reveal_panel = frame.panel;
        } else {
            scene.overlay.notification_reveal_regions.clear();
            scene.overlay.notification_reveal_panel = None;
        }
    }

    pub(super) fn paint_notification_reveal(
        &mut self,
        scene: &SceneGraph,
        candidates: &[RevealCandidate],
        rects: &mut Vec<RoundedRectDrawCmd>,
        items: &mut Vec<TextItem>,
    ) -> Option<NotificationRevealPanel> {
        let request = scene.overlay.notification_reveal?;
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.region.target == request.target)?;
        if scene
            .overlay
            .notification_choices
            .contains_key(&request.target.publication_id)
        {
            return None;
        }
        let card = candidate.card;
        let display = self
            .display_layout
            .displays()
            .iter()
            .find(|display| {
                display
                    .rect
                    .contains_point(card.x + card.width * 0.5, card.y + card.height * 0.5)
            })
            .map_or(scene.display_area, |display| display.rect);
        let tokens = self.zone_tokens(&candidate.token_key);
        let gap = dp(tokens, "space.xs");
        let padding = dp(tokens, "space.s");
        let width = card.width.min(display.width);
        let text_width = width - 2.0 * padding;
        let size = text::clamp_raw_font_size(dp(tokens, "type.notification.body.size"));
        let line = size * text::LINE_HEIGHT_MULTIPLIER;
        let max_height = dp(tokens, "notification.reveal.max_height").min(display.height);
        let min_height = 2.0 * padding + line;
        if text_width <= 0.0 || max_height < min_height {
            return None;
        }
        let radius = dp(tokens, "shape.m");
        let stroke = dp(tokens, "stroke.border.width");
        let fill = color(tokens, "color.surface.container.high");
        let border = color(tokens, "color.outline.variant");
        let text_color = color(tokens, "color.on_surface");
        let focus_width = dp(tokens, "focus.ring.width");
        let focus_color = color(tokens, "color.primary");
        let content_height = self
            .notification_reveal_measure
            .as_ref()
            .filter(|measure| {
                measure.target == request.target
                    && measure.width == text_width
                    && measure.text_size == size
            })
            .map(|measure| measure.height)
            .unwrap_or_else(|| {
                text::measure_raw_content_height(
                    self.text_rasterizer
                        .as_mut()
                        .expect("reveal candidates require text")
                        .font_system_mut(),
                    &candidate.text,
                    text_width,
                    size,
                    candidate.font,
                )
            });
        self.notification_reveal_measure = Some(RevealMeasure {
            target: request.target,
            width: text_width,
            text_size: size,
            height: content_height,
        });
        let wanted_height = (content_height + 2.0 * padding).clamp(min_height, max_height);
        let bounds = placement(
            display,
            card,
            candidate.region.bounds,
            (width, wanted_height),
            gap,
            min_height,
        )?;
        let viewport = Rect::new(
            bounds.x + padding,
            bounds.y + padding,
            text_width,
            bounds.height - 2.0 * padding,
        );
        let max_scroll = (content_height - viewport.height).max(0.0);
        let mut item = Self::make_zone_text_item(
            Arc::clone(&candidate.text),
            viewport.x,
            viewport.y - request.scroll.clamp(0.0, max_scroll),
            viewport.width,
            f32::MAX,
            size,
            candidate.font,
            400,
            text::rgba_to_srgb_u8(text_color),
            TextAlign::Start,
            TextOverflow::Clip,
            None,
            None,
            1.0,
        );
        item.clip_pixel_y = viewport.y;
        item.clip_bounds_height = viewport.height;
        if !matches!(request.target.field, NotificationRevealField::Action(_))
            && scene.overlay.notification_focus.as_ref() == Some(&candidate.region.focus_id)
        {
            rects.push(self.border_only_cmd(
                candidate.region.bounds,
                radius,
                focus_width,
                focus_color,
            ));
        }
        rects.push(RoundedRectDrawCmd {
            x: bounds.x,
            y: bounds.y,
            width: bounds.width,
            height: bounds.height,
            radius,
            color: self.gpu_color(fill),
            border: Some(RoundedRectBorder {
                width: stroke,
                color: self.gpu_color(border),
            }),
            clip: None,
        });
        items.push(item);
        Some(NotificationRevealPanel {
            target: request.target,
            bounds,
            bridge: pointer_bridge(candidate.region.bounds, bounds),
            max_scroll,
        })
    }
}
