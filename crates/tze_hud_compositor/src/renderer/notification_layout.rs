//! Retained notification geometry. Measurement uses the same font roles,
//! advanced shaping as the text rasterizer; unchanged frames only
//! look up the publication's descriptor.
use super::token_colors::{
    NOTIFICATION_ACTION_BUTTON_H_PX, NOTIFICATION_ACTION_ROW_GAP_PX, notification_inset,
    resolve_notification_action_tokens,
};
use std::collections::HashMap;
use tze_hud_scene::types::*;

#[derive(Clone, Debug, Default)]
pub(super) struct NotificationActionLayout {
    pub(super) stacked: bool,
    pub(super) heights: Vec<f32>,
    pub(super) gap: f32,
    pub(super) padding: f32,
}
impl NotificationActionLayout {
    pub(super) fn reserve(&self, inset: f32) -> f32 {
        if self.heights.is_empty() {
            return 0.0;
        }
        self.gap
            + inset
            + if self.stacked {
                self.heights.iter().sum::<f32>()
                    + self.gap * self.heights.len().saturating_sub(1) as f32
            } else {
                self.heights[0]
            }
    }
    pub(super) fn bounds(&self, x: f32, y: f32, w: f32, h: f32, inset: f32) -> Vec<Rect> {
        let n = self.heights.len();
        if n == 0 {
            return Vec::new();
        }
        let available = (w - inset * 2.0).max(1.0);
        let width = if self.stacked {
            available
        } else {
            available / n as f32
        };
        let mut top = (y + h - self.reserve(inset) + self.gap).max(y);
        self.heights
            .iter()
            .enumerate()
            .map(|(i, height)| {
                let rect = Rect::new(
                    x + inset + if self.stacked { 0.0 } else { i as f32 * width },
                    top,
                    width,
                    (*height).min((y + h - inset - top).max(0.0)),
                );
                if self.stacked {
                    top += height + self.gap;
                }
                rect
            })
            .collect()
    }
}

#[derive(PartialEq)]
struct LayoutInputs {
    labels: Vec<String>,
    width: f32,
    inset: f32,
    font: FontFamily,
    size: f32,
    weight: u16,
    minimum_height: f32,
    gap: f32,
    padding: f32,
}
#[derive(Default)]
pub(super) struct NotificationLayoutCache {
    entries: HashMap<(String, SceneId), (LayoutInputs, NotificationActionLayout, bool)>,
    config: Option<crate::fonts::FontConfig>,
    fonts: Option<glyphon::FontSystem>,
    pub(super) shapes: u64,
}
impl NotificationLayoutCache {
    pub(super) fn resolve(
        &mut self,
        tokens: &HashMap<String, String>,
        zone: &str,
        record: &ZonePublishRecord,
        policy: &RenderingPolicy,
        width: f32,
    ) -> NotificationActionLayout {
        self.resolve_with_fonts(tokens, zone, record, policy, width, None)
    }
    pub(super) fn resolve_with_fonts(
        &mut self,
        tokens: &HashMap<String, String>,
        zone: &str,
        record: &ZonePublishRecord,
        policy: &RenderingPolicy,
        width: f32,
        actual_fonts: Option<&mut glyphon::FontSystem>,
    ) -> NotificationActionLayout {
        let ZoneContent::Notification(payload) = &record.content else {
            return NotificationActionLayout::default();
        };
        if payload.actions.is_empty() {
            return NotificationActionLayout::default();
        }
        let config = crate::fonts::FontConfig::from_token_map(tokens).normalized();
        if self.config.as_ref() != Some(&config) {
            self.entries.clear();
            self.fonts = None;
            self.config = Some(config.clone());
        }
        let action = resolve_notification_action_tokens(tokens);
        let scale = tze_hud_config::tokens::px_scale(tokens);
        let metric =
            |name: &str, fallback| tze_hud_config::tokens::token_px(tokens, name, fallback);
        let inputs = LayoutInputs {
            labels: payload
                .actions
                .iter()
                .take(MAX_NOTIFICATION_ACTIONS)
                .map(|a| a.label.clone())
                .collect(),
            width,
            inset: notification_inset(policy, scale),
            font: policy.font_family.unwrap_or(FontFamily::SystemSansSerif),
            size: action.font_size_px,
            weight: action.font_weight,
            minimum_height: metric(
                "notification.action.height_px",
                NOTIFICATION_ACTION_BUTTON_H_PX,
            ),
            gap: metric("notification.action.gap_px", NOTIFICATION_ACTION_ROW_GAP_PX),
            padding: metric("notification.action.padding_px", 2.0),
        };
        let key = (zone.to_owned(), record.publication_id);
        let actual = actual_fonts.is_some();
        if let Some((previous, layout, from_actual)) = self.entries.get(&key)
            && previous == &inputs
            && (!actual || *from_actual)
        {
            return layout.clone();
        }
        let fonts = match actual_fonts {
            Some(fonts) => fonts,
            None => self
                .fonts
                .get_or_insert_with(|| crate::fonts::build_font_system(&config).0),
        };
        let width = (width - 2.0 * inputs.inset).max(1.0);
        let cell = width / inputs.labels.len().max(1) as f32;
        let mut measured = 0;
        let stacked = inputs.labels.iter().any(|label| {
            if label.contains('\n') {
                return true;
            }
            measured += 1;
            crate::text::single_line_width(fonts, label, inputs.size, inputs.font, inputs.weight)
                + inputs.padding * 2.0
                > cell
        });
        // Labels retain the existing bounded Clip policy after row/stack
        // selection. Unbounded text must not push the fourth option away.
        self.shapes += measured;
        let height = (inputs.size * 1.4 + inputs.padding * 2.0).max(inputs.minimum_height);
        let heights = vec![height; inputs.labels.len()];
        let layout = NotificationActionLayout {
            stacked,
            heights,
            gap: inputs.gap,
            padding: inputs.padding,
        };
        self.entries.insert(key, (inputs, layout.clone(), actual));
        layout
    }
    #[cfg(test)]
    pub(super) fn invalidate(&mut self) {
        self.entries.clear();
    }
    pub(super) fn retain(&mut self, scene: &tze_hud_scene::graph::SceneGraph) {
        self.entries.retain(|(zone, id), _| {
            scene
                .zone_registry
                .active_for_zone(zone)
                .iter()
                .any(|record| record.publication_id == *id)
        });
    }
}
