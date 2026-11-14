//! Retained notification geometry. Measurement uses the same font roles,
//! advanced shaping as the text rasterizer; unchanged frames only
//! look up the publication's descriptor.
use super::notification_style::NotificationStyle;
use super::token_colors::{
    NOTIFICATION_ACTION_BUTTON_H_PX, NOTIFICATION_ACTION_ROW_GAP_PX, is_alert_banner_zone,
    notification_inset, resolve_notification_action_tokens,
};
use std::collections::HashMap;
use tze_hud_scene::types::*;

#[derive(Clone, Debug, Default)]
pub(super) struct NotificationActionLayout {
    pub(super) stacked: bool,
    pub(super) heights: Vec<f32>,
    /// Between stacked actions.
    pub(super) gap: f32,
    /// Between side-by-side actions.
    pub(super) row_spacing: f32,
    /// Between the text and the first action.
    pub(super) row_gap: f32,
    /// Card inset of the action row from the card's sides, and from its
    /// bottom.
    pub(super) inset: f32,
    pub(super) inset_bottom: f32,
    /// Horizontal label padding inside a button.
    pub(super) padding: f32,
    /// Label box offset from the button top, and its height.
    pub(super) label_y: f32,
    pub(super) label_height: f32,
    /// Label `TextItem::line_height_multiplier`.
    pub(super) label_line_multiplier: Option<f32>,
    /// Reserved in both idle and focused cards so digit hints never reflow labels.
    pub(super) hint_width: f32,
    pub(super) link_marks: Vec<Option<(String, f32)>>,
}
impl NotificationActionLayout {
    fn rows_height(&self) -> f32 {
        if self.stacked {
            self.heights.iter().sum::<f32>()
                + self.gap * self.heights.len().saturating_sub(1) as f32
        } else {
            self.heights.first().copied().unwrap_or(0.0)
        }
    }
    /// Height the action row adds below the text block.
    pub(super) fn reserve(&self) -> f32 {
        if self.heights.is_empty() {
            return 0.0;
        }
        self.row_gap + self.rows_height()
    }
    /// The label's clip width inside button `bound`: the button minus its
    /// padding, the reserved digit hint and any link mark.
    pub(super) fn label_width(&self, bound: &Rect, index: usize) -> f32 {
        let available = (bound.width - self.padding * 2.0).max(1.0);
        let hint_width = self.hint_width.min(available);
        let mark_width = self
            .link_marks
            .get(index)
            .and_then(Option::as_ref)
            .map_or(0.0, |(_, width)| {
                (*width).min((available - hint_width).max(0.0))
            });
        (available - hint_width - mark_width).max(1.0)
    }
    /// Button bounds in a card at (`x`, `y`, `w`, `h`), [`Self::inset`] from
    /// its bottom and sides. The single source for drawing and hit testing.
    pub(super) fn bounds(&self, x: f32, y: f32, w: f32, h: f32) -> Vec<Rect> {
        let inset = self.inset;
        let n = self.heights.len();
        if n == 0 {
            return Vec::new();
        }
        let available = (w - inset * 2.0).max(1.0);
        let width = if self.stacked {
            available
        } else {
            ((available - self.row_spacing * (n - 1) as f32) / n as f32).max(1.0)
        };
        let bottom = self.inset_bottom;
        let mut top = (y + h - bottom - self.rows_height()).max(y);
        self.heights
            .iter()
            .enumerate()
            .map(|(i, height)| {
                let rect = Rect::new(
                    x + inset
                        + if self.stacked {
                            0.0
                        } else {
                            i as f32 * (width + self.row_spacing)
                        },
                    top,
                    width,
                    (*height).min((y + h - bottom - top).max(0.0)),
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
    link_marks: Vec<Option<String>>,
    width: f32,
    inset: f32,
    inset_bottom: f32,
    font: FontFamily,
    size: f32,
    weight: u16,
    minimum_height: f32,
    line_height: Option<f32>,
    gap: f32,
    row_spacing: f32,
    row_gap: f32,
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
    /// Shared font database for callers measuring without a rasterizer
    /// (admission-time fit); rebuilt when the font roles change.
    pub(super) fn fallback_fonts(
        &mut self,
        tokens: &HashMap<String, String>,
    ) -> &mut glyphon::FontSystem {
        let config = crate::fonts::FontConfig::from_token_map(tokens).normalized();
        if self.config.as_ref() != Some(&config) {
            self.entries.clear();
            self.fonts = None;
            self.config = Some(config.clone());
        }
        self.fonts
            .get_or_insert_with(|| crate::fonts::build_font_system(&config).0)
    }
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
        let labels = payload
            .actions
            .iter()
            .take(MAX_NOTIFICATION_ACTIONS)
            .map(|a| super::token_colors::notification_action_label(tokens, a))
            .collect();
        let link_marks = payload
            .actions
            .iter()
            .take(MAX_NOTIFICATION_ACTIONS)
            .map(|a| {
                a.url.as_ref().map(|_| {
                    format!(
                        " {}",
                        tokens
                            .get("notification.action.link_mark")
                            .map(String::as_str)
                            .unwrap_or("↗")
                    )
                })
            })
            .collect();
        let font = policy.font_family.unwrap_or(FontFamily::SystemSansSerif);
        let inputs = if is_alert_banner_zone(zone) {
            // Pre-restyle alert-banner metrics (hud-h51u7.4.2 restyles them).
            let action = resolve_notification_action_tokens(tokens);
            let scale = tze_hud_config::tokens::px_scale(tokens);
            let metric =
                |name: &str, fallback| tze_hud_config::tokens::token_px(tokens, name, fallback);
            let inset = notification_inset(policy, scale);
            let gap = metric("notification.action.gap_px", NOTIFICATION_ACTION_ROW_GAP_PX);
            LayoutInputs {
                labels,
                link_marks,
                width,
                inset,
                inset_bottom: inset,
                font,
                size: action.font_size_px,
                weight: action.font_weight,
                minimum_height: metric(
                    "notification.action.height_px",
                    NOTIFICATION_ACTION_BUTTON_H_PX,
                ),
                line_height: None,
                gap,
                row_spacing: 0.0,
                row_gap: gap + inset,
                padding: metric("notification.action.padding_px", 2.0),
            }
        } else {
            let style = NotificationStyle::resolve(tokens, policy);
            LayoutInputs {
                labels,
                link_marks,
                width,
                inset: style.padding_h,
                inset_bottom: style.padding_v,
                font,
                size: style.action_label.size,
                weight: style.action_label.weight,
                minimum_height: style.action_height,
                line_height: Some(style.action_label.line_height),
                gap: style.action_gap,
                row_spacing: style.action_gap,
                row_gap: style.action_row_gap,
                padding: style.action_padding,
            }
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
        let count = inputs.labels.len().max(1) as f32;
        let cell = (width - inputs.row_spacing * (count - 1.0)) / count;
        let hint_width = ["1", "2", "3", "4"]
            .into_iter()
            .map(|digit| {
                self.shapes += 1;
                crate::text::single_line_width(
                    fonts,
                    digit,
                    inputs.size,
                    inputs.font,
                    inputs.weight,
                )
            })
            .fold(0.0_f32, f32::max)
            + inputs.padding;
        let mut measured = 0;
        let stacked = inputs.labels.iter().any(|label| {
            if label.contains('\n') {
                return true;
            }
            measured += 1;
            crate::text::single_line_width(fonts, label, inputs.size, inputs.font, inputs.weight)
                + inputs.padding * 2.0
                + hint_width
                > cell
        });
        // Labels retain the existing bounded Clip policy after row/stack
        // selection. Unbounded text must not push the fourth option away.
        self.shapes += measured;
        // Cards size buttons from tokens and centre the label line; alert
        // banners keep their legacy padded label box.
        let (height, label_y, label_height) = match inputs.line_height {
            Some(line) => {
                let height = inputs.minimum_height.max(line);
                (height, (height - line) * 0.5, line)
            }
            None => {
                let height = (inputs.size * 1.4 + inputs.padding * 2.0).max(inputs.minimum_height);
                (
                    height,
                    inputs.padding,
                    (height - inputs.padding * 2.0).max(1.0),
                )
            }
        };
        let heights = vec![height; inputs.labels.len()];
        let link_marks = inputs
            .link_marks
            .iter()
            .map(|mark| {
                mark.as_ref().map(|mark| {
                    self.shapes += 1;
                    (
                        mark.clone(),
                        crate::text::single_line_width(
                            fonts,
                            mark,
                            inputs.size,
                            inputs.font,
                            inputs.weight,
                        ),
                    )
                })
            })
            .collect();
        let layout = NotificationActionLayout {
            stacked,
            link_marks,
            heights,
            gap: inputs.gap,
            row_spacing: inputs.row_spacing,
            row_gap: inputs.row_gap,
            inset: inputs.inset,
            inset_bottom: inputs.inset_bottom,
            padding: inputs.padding,
            label_y,
            label_height,
            label_line_multiplier: inputs.line_height.map(|line| line / inputs.size.max(1.0)),
            hint_width,
        };
        self.entries.insert(key, (inputs, layout.clone(), actual));
        layout
    }
    /// Forget every layout (a font change alters label widths).
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
