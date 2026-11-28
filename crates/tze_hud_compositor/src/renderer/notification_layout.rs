//! Retained notification geometry. Measurement uses the same font roles,
//! advanced shaping as the text rasterizer. Unchanged frames look up the
//! publication's descriptor.
use super::notification_style::NotificationStyle;
use std::collections::HashMap;
use tze_hud_scene::graph::SceneGraph;
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
    reply: Option<(usize, f32)>,
}
impl NotificationActionLayout {
    pub(super) fn with_reply(
        mut self,
        scene: &SceneGraph,
        record: &ZonePublishRecord,
        tokens: &HashMap<String, String>,
    ) -> Self {
        if let Some(reply) = &scene.overlay.notification_reply
            && reply.publication_id == record.publication_id
            && reply.action_index < self.heights.len()
        {
            let composer = super::notification_reply::reply_style(tokens);
            let line_height =
                (composer.font_size_px * crate::text::LINE_HEIGHT_MULTIPLIER).max(1.0);
            self.reply = Some((
                reply.action_index,
                line_height + 2.0 * composer.content_inset_px,
            ));
        }
        self
    }

    fn reply_reserve(&self) -> f32 {
        self.reply.map_or(0.0, |(_, height)| height + self.gap)
    }

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
        self.row_gap + self.rows_height() + self.reply_reserve()
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
        let mut top = (y + h - bottom - self.rows_height() - self.reply_reserve()).max(y);
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
                    if self.reply.is_some_and(|(index, _)| index == i) {
                        top += self.reply_reserve();
                    }
                }
                rect
            })
            .collect()
    }

    pub(super) fn reply_bounds(&self, card: Rect, buttons: &[Rect]) -> Option<Rect> {
        let (index, height) = self.reply?;
        let button = buttons.get(index)?;
        Some(Rect::new(
            card.x + self.inset,
            button.y + button.height + self.gap,
            (card.width - 2.0 * self.inset).max(1.0),
            height,
        ))
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
    line_height: f32,
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
    /// Whether the reply layout came from the rasterizer's fonts, which hold
    /// the lazily loaded system fallback faces (CJK, emoji).
    reply_line: Option<(String, f32, tze_hud_input::ComposerVisualLayout, bool)>,
    pub(super) shapes: u64,
}
impl NotificationLayoutCache {
    /// Shared font database for callers measuring without a rasterizer
    /// (admission-time fit); rebuilt when the font roles change.
    pub(super) fn fallback_fonts(
        &mut self,
        tokens: &HashMap<String, String>,
    ) -> &mut glyphon::FontSystem {
        let config = self.sync_config(tokens);
        self.fonts
            .get_or_insert_with(|| crate::fonts::build_font_system(&config).0)
    }

    /// Drop everything shaped under a different font configuration.
    fn sync_config(&mut self, tokens: &HashMap<String, String>) -> crate::fonts::FontConfig {
        let config = crate::fonts::FontConfig::from_token_map(tokens).normalized();
        if self.config.as_ref() != Some(&config) {
            self.entries.clear();
            self.fonts = None;
            self.reply_line = None;
            self.config = Some(config.clone());
        }
        config
    }

    /// The reply draft's layout. The commit-time prime passes the
    /// rasterizer's `actual` fonts, so later reads match the drawn glyphs.
    pub(super) fn reply_line(
        &mut self,
        tokens: &HashMap<String, String>,
        text: &str,
        font_size: f32,
        actual: Option<&mut glyphon::FontSystem>,
    ) -> &tze_hud_input::ComposerVisualLayout {
        self.sync_config(tokens);
        if self
            .reply_line
            .as_ref()
            .is_none_or(|(content, size, _, from_actual)| {
                content != text || *size != font_size || (actual.is_some() && !from_actual)
            })
        {
            let from_actual = actual.is_some();
            let fonts = match actual {
                Some(fonts) => fonts,
                None => self.fallback_fonts(tokens),
            };
            let layout = crate::text::measure_editor_line(
                fonts,
                text,
                font_size,
                crate::text::LINE_HEIGHT_MULTIPLIER,
            );
            self.reply_line = Some((text.to_owned(), font_size, layout, from_actual));
        }
        &self.reply_line.as_ref().expect("reply layout primed").2
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
        let config = self.sync_config(tokens);
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
        let style = NotificationStyle::for_zone(zone, tokens, policy);
        let inputs = LayoutInputs {
            labels,
            link_marks,
            width,
            inset: style.padding_h,
            inset_bottom: style.padding_v,
            font,
            size: style.action_label.size,
            weight: style.action_label.weight,
            minimum_height: style.action_height,
            line_height: style.action_label.line_height,
            gap: style.action_gap,
            row_spacing: style.action_gap,
            row_gap: style.action_row_gap,
            padding: style.action_padding,
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
        // A link label already carries its mark (`notification_action_label`),
        // so this matches `label_width`, which subtracts the mark instead.
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
        // Labels ellipsize within the selected button widths. Long text
        // cannot push later options away.
        self.shapes += measured;
        // Buttons size from tokens and centre the label line.
        let line = inputs.line_height;
        let height = inputs.minimum_height.max(line);
        let (label_y, label_height) = ((height - line) * 0.5, line);
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
            label_line_multiplier: Some(inputs.line_height / inputs.size.max(1.0)),
            hint_width,
            reply: None,
        };
        self.entries.insert(key, (inputs, layout.clone(), actual));
        layout
    }
    /// Forget every layout (a font change alters label widths).
    pub(super) fn invalidate(&mut self) {
        self.entries.clear();
        self.reply_line = None;
    }
    pub(super) fn retain(&mut self, scene: &tze_hud_scene::graph::SceneGraph) {
        if scene.overlay.notification_reply.is_none() {
            self.reply_line = None;
        }
        self.entries.retain(|(zone, id), _| {
            scene
                .zone_registry
                .active_for_zone(zone)
                .iter()
                .any(|record| record.publication_id == *id)
        });
    }
}
