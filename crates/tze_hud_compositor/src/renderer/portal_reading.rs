//! Portal OUTPUT reading decoration (hud-ljhku.2): selection highlight and
//! link underline behind the transcript glyphs, plus the code copy button and
//! the link-target hint on the scroll-affordance layer above them.
//!
//! The runtime owns the state ([`PortalReadingDecor`]); geometry comes from
//! the shaped document the compositor already published. Every color is a
//! design token; a decoration change repaints but never reshapes.

use std::sync::Arc;

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{FontFamily, PortalPartKind, Rect, Rgba, TextAlign, TextOverflow, Tile};
use tze_hud_scene::{PortalDocumentLayout, PortalReadingDecor, SceneId};

use super::Compositor;
use super::token_colors::{
    resolve_composer_overlay_tokens, resolve_jump_to_latest_tokens, resolve_token_color,
    srgb_to_linear,
};
use crate::pipeline::{RectVertex, RoundedRectBorder, RoundedRectDrawCmd};
use crate::text::TextItem;

/// Token-resolved reading colors, all linear.
pub(crate) struct PortalReadingTokens {
    /// `portal.transcript.selection_color`, else the composer selection color.
    pub selection: Rgba,
    /// `portal.transcript.link_color`, else `color.link.text`, else the transcript text.
    pub link: Rgba,
    /// `portal.focus_ring.color` for a keyboard-focused copy button.
    pub focus: Option<Rgba>,
}

pub(crate) fn resolve_portal_reading_tokens(
    map: &std::collections::HashMap<String, String>,
) -> PortalReadingTokens {
    let linear = |c: tze_hud_config::tokens::Rgba| Rgba {
        r: srgb_to_linear(c.r),
        g: srgb_to_linear(c.g),
        b: srgb_to_linear(c.b),
        a: c.a,
    };
    let selection = resolve_token_color(
        map,
        tze_hud_config::portal_tokens::PORTAL_TOKEN_TRANSCRIPT_SELECTION_COLOR,
    )
    .unwrap_or_else(|| {
        let [r, g, b, a] = resolve_composer_overlay_tokens(map).selection_bg;
        Rgba {
            r: srgb_to_linear(f32::from(r) / 255.0),
            g: srgb_to_linear(f32::from(g) / 255.0),
            b: srgb_to_linear(f32::from(b) / 255.0),
            a: f32::from(a) / 255.0,
        }
    });
    let link = resolve_token_color(map, "portal.transcript.link_color")
        .or_else(|| resolve_token_color(map, "color.link.text"))
        .unwrap_or_else(|| {
            linear(tze_hud_config::resolve_portal_tokens(map).transcript_text_color)
        });
    PortalReadingTokens {
        selection,
        link,
        focus: resolve_token_color(map, "portal.focus_ring.color"),
    }
}

/// The OUTPUT layout `node_id` was shaped into, when it carries decoration.
fn decorated(
    scene: &SceneGraph,
    tile: SceneId,
    node_id: Option<SceneId>,
) -> Option<(&PortalReadingDecor, &PortalDocumentLayout)> {
    let decor = scene.overlay.portal_reading.get(&tile)?;
    let layout = scene
        .overlay
        .portal_document_layouts
        .get(&(tile, PortalPartKind::Transcript))
        .filter(|l| node_id.is_none_or(|id| l.node_id == id))?;
    Some((decor, layout))
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let (x, y) = (a.x.max(b.x), a.y.max(b.y));
    let (r, btm) = (
        (a.x + a.width).min(b.x + b.width),
        (a.y + a.height).min(b.y + b.height),
    );
    (r > x && btm > y).then(|| Rect::new(x, y, r - x, btm - y))
}

/// Underline thickness for a document line height (a ratio, not a pixel value).
pub(crate) fn underline_thickness(line_height: f32) -> f32 {
    (line_height / 16.0).round().max(1.0)
}

impl Compositor {
    /// Screen rectangles and colors of `tile`'s selection highlight and link
    /// underline, clipped to the OUTPUT viewport, at the text's scroll offset.
    fn portal_reading_rects(
        &self,
        tile: &Tile,
        node_id: Option<SceneId>,
        scene: &SceneGraph,
        scroll_y: f32,
    ) -> Vec<(Rect, Rgba)> {
        let Some((decor, layout)) = decorated(scene, tile.id, node_id) else {
            return Vec::new();
        };
        let tokens = resolve_portal_reading_tokens(&self.token_map);
        let view = Rect::new(
            tile.bounds.x + layout.viewport.x,
            tile.bounds.y + layout.viewport.y,
            layout.viewport.width,
            layout.viewport.height,
        );
        let place = |r: Rect| Rect::new(view.x + r.x, view.y + r.y - scroll_y, r.width, r.height);
        let mut out = Vec::new();
        if let Some((start, end)) = decor.selection {
            out.extend(
                layout
                    .range_rects(start, end)
                    .into_iter()
                    .filter_map(|r| intersect(place(r), view))
                    .map(|r| (r, tokens.selection)),
            );
        }
        if let Some((start, end)) = decor.link {
            let t = underline_thickness(layout.line_height);
            out.extend(
                layout
                    .range_rects(start, end)
                    .into_iter()
                    .map(place)
                    .filter_map(|r| intersect(Rect::new(r.x, r.y + r.height - t, r.width, t), view))
                    .map(|r| (r, tokens.link)),
            );
        }
        out
    }

    /// Degraded (flat) path: selection and underline quads for OUTPUT node
    /// `node_id`, after its flat backdrop and behind its glyphs.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn append_portal_reading_quads(
        &self,
        tile: &Tile,
        node_id: SceneId,
        scene: &SceneGraph,
        scroll_y: f32,
        opacity: f32,
        sw: f32,
        sh: f32,
        vertices: &mut Vec<RectVertex>,
    ) {
        if self.degradation_policy.level < tze_hud_scene::DegradationLevel::Simplified {
            return; // drawn by `collect_portal_reading_rects` above the rounded panes
        }
        for (rect, color) in self.portal_reading_rects(tile, Some(node_id), scene, scroll_y) {
            let color = self.gpu_color(Rgba {
                a: color.a * opacity,
                ..color
            });
            Self::append_clipped_rect_vertices(tile, rect, sw, sh, color, vertices);
        }
    }

    /// Selection and underline as content rounded rects, drawn after the
    /// tile's rounded pane backdrops and before its glyphs.
    pub(super) fn collect_portal_reading_rects(
        &self,
        tile: &Tile,
        scene: &SceneGraph,
        scroll_y: f32,
        opacity: f32,
        cmds: &mut Vec<RoundedRectDrawCmd>,
    ) {
        for (rect, color) in self.portal_reading_rects(tile, None, scene, scroll_y) {
            cmds.push(RoundedRectDrawCmd {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                radius: 0.0,
                color: self.gpu_color(Rgba {
                    a: color.a * opacity,
                    ..color
                }),
                border: None,
                clip: None,
            });
        }
    }

    /// Copy button and link hint pills with their labels, above OUTPUT text.
    pub(super) fn collect_portal_reading_affordances(
        &self,
        tile: &Tile,
        scene: &SceneGraph,
        rects: &mut Vec<RoundedRectDrawCmd>,
        items: &mut Vec<TextItem>,
    ) {
        let Some((decor, _)) = decorated(scene, tile.id, None) else {
            return;
        };
        let pill = resolve_jump_to_latest_tokens(&self.token_map);
        let reading = resolve_portal_reading_tokens(&self.token_map);
        let opacity = self.tile_effective_opacity(tile, scene);
        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        let text_color = [pill.text_r, pill.text_g, pill.text_b, pill.text_a].map(to_u8);
        let link_color = [
            super::token_colors::linear_to_srgb(reading.link.r),
            super::token_colors::linear_to_srgb(reading.link.g),
            super::token_colors::linear_to_srgb(reading.link.b),
            reading.link.a,
        ]
        .map(to_u8);
        let mut pill_at = |bounds: Rect, label: &str, color: [u8; 4], focused: bool| {
            let mut fill =
                self.gpu_color_raw([pill.color_r, pill.color_g, pill.color_b, pill.color_a]);
            fill[3] *= opacity;
            rects.push(RoundedRectDrawCmd {
                x: bounds.x,
                y: bounds.y,
                width: bounds.width,
                height: bounds.height,
                radius: bounds.height / 4.0,
                color: fill,
                border: focused
                    .then_some(reading.focus)
                    .flatten()
                    .map(|c| RoundedRectBorder {
                        width: underline_thickness(bounds.height) * 2.0,
                        color: self.gpu_color(c),
                    }),
                clip: None,
            });
            if self.text_rasterizer.is_none() {
                return;
            }
            let font_size_px = pill.text_size_px.max(1.0);
            let line_height_multiplier =
                crate::markdown::MarkdownTokens::default().line_height_multiplier;
            let line = font_size_px * line_height_multiplier;
            items.push(TextItem {
                text: Arc::from(label),
                pixel_x: bounds.x,
                pixel_y: bounds.y + ((bounds.height - line) / 2.0).max(0.0),
                bounds_width: bounds.width.max(1.0),
                bounds_height: bounds.height.max(1.0),
                clip_pixel_x: bounds.x,
                clip_pixel_y: bounds.y,
                clip_bounds_width: bounds.width.max(1.0),
                clip_bounds_height: bounds.height.max(1.0),
                font_size_px,
                font_family: FontFamily::SystemSansSerif,
                font_weight: 400,
                color,
                alignment: TextAlign::Center,
                overflow: TextOverflow::Clip,
                outline_color: None,
                outline_width: None,
                opacity,
                color_runs: Box::new([]),
                styled_runs: Box::new([]),
                line_height_multiplier,
                viewport: crate::overflow::TruncationViewport::HeadAnchored,
            });
        };
        let at = |r: Rect| Rect::new(tile.bounds.x + r.x, tile.bounds.y + r.y, r.width, r.height);
        if let Some(button) = &decor.copy_button {
            let (label, color) = if button.copied {
                ("Copied", link_color)
            } else {
                ("Copy", text_color)
            };
            pill_at(at(button.bounds), label, color, button.focused);
        }
        if let Some((host, anchor)) = &decor.hint {
            pill_at(at(*anchor), host, text_color, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_selection_defaults_to_the_composer_selection_color() {
        let mut map = std::collections::HashMap::new();
        let default = resolve_portal_reading_tokens(&map).selection;
        map.insert("portal.composer.selection_color".into(), "#FF000080".into());
        let composer = resolve_portal_reading_tokens(&map).selection;
        assert!(composer.r > 0.9 && composer.g < 0.01 && (composer.a - 0.5).abs() < 0.01);
        assert!(default.b > default.r, "calm blue default");
        map.insert("portal.transcript.selection_color".into(), "#00FF00".into());
        let own = resolve_portal_reading_tokens(&map).selection;
        assert!(own.g > 0.9 && own.r < 0.01, "the transcript key wins");
    }

    #[test]
    fn link_underline_uses_the_link_token_then_text_color() {
        let mut map = std::collections::HashMap::new();
        map.insert("portal.transcript.link_color".into(), "#0000FF".into());
        let link = resolve_portal_reading_tokens(&map).link;
        assert!(link.b > 0.9 && link.r < 0.01);
        map.clear();
        assert!(resolve_portal_reading_tokens(&map).link.a > 0.0);
        assert_eq!(underline_thickness(20.0), 1.0);
        assert_eq!(underline_thickness(40.0), 3.0);
    }
}
