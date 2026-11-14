//! Admission-time text fit (`docs/api.md` "clipped").
//!
//! The compositor keeps a [`TextFitMeasurer`] in step with what it draws
//! with: the token map, the display layout, and the token-resolved widget
//! SVGs. MCP admission asks it which published text would be cut, using the
//! same card geometry ([`super::notification_text`], the slot layout and the
//! action-row layout) and the same shapers: cosmic-text ellipsis truncation
//! for card text, and usvg text layout over the widget font set for widget
//! labels. Runtime-uploaded fonts are not loaded here; cards using one are
//! measured with the role's fallback face.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;

use glyphon::{Attrs, Family, Weight};
use tze_hud_scene::SceneId;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{
    ContentionPolicy, FontFamily, NotificationPayload, WidgetBinding, WidgetParameterValue,
    ZoneContent, ZonePublishRecord,
};

use super::Compositor;
use super::notification_layout::NotificationLayoutCache;
use super::notification_text::notification_text_layout;
use super::token_colors::{notification_inset, resolve_notification_action_tokens};
use crate::display::DisplayLayout;

/// The compositor's current drawing inputs, shared with MCP admission.
/// Antialiasing slack: a label ending on its clip edge is not cut.
const EDGE_SLACK_PX: f32 = 0.5;

#[derive(Default)]
pub struct TextFitMeasurer {
    state: Mutex<FitState>,
}

#[derive(Default)]
struct FitState {
    /// `None` until a compositor attaches: nothing is reported before then.
    tokens: Option<HashMap<String, String>>,
    layout: DisplayLayout,
    /// Token-resolved widget SVG sources keyed by (widget type, file).
    widget_svgs: HashMap<(String, String), String>,
    /// Action-row layouts and the shared fallback font database.
    cards: RefCell<NotificationLayoutCache>,
}

impl std::fmt::Debug for TextFitMeasurer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextFitMeasurer").finish_non_exhaustive()
    }
}

impl TextFitMeasurer {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FitState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The token map the compositor now draws with.
    pub fn set_tokens(&self, tokens: HashMap<String, String>) {
        self.state().tokens = Some(tokens);
    }

    /// The display layout zones resolve against.
    pub fn set_display_layout(&self, layout: DisplayLayout) {
        self.state().layout = layout;
    }

    /// A token-resolved widget SVG layer, as registered for drawing.
    pub fn register_widget_svg(&self, widget_type: &str, svg_file: &str, svg: &[u8]) {
        if let Ok(svg) = std::str::from_utf8(svg) {
            self.state().widget_svgs.insert(
                (widget_type.to_owned(), svg_file.to_owned()),
                svg.to_owned(),
            );
        }
    }
}

impl tze_hud_scene::TextFit for TextFitMeasurer {
    fn notification(
        &self,
        scene: &SceneGraph,
        zone: &str,
        payload: &NotificationPayload,
    ) -> Vec<&'static str> {
        let state = self.state();
        let Some(tokens) = &state.tokens else {
            return Vec::new();
        };
        let Some(zone_def) = scene.zone_registry.zones.get(zone) else {
            return Vec::new();
        };
        // Only Stack zones draw notification cards.
        if !matches!(zone_def.contention_policy, ContentionPolicy::Stack { .. }) {
            return Vec::new();
        }
        let zone_tokens = tze_hud_config::tokens::for_zone(tokens, zone);
        let scale = tze_hud_config::tokens::px_scale(&zone_tokens);
        let policy = &zone_def.rendering_policy;
        let (_, zy, zw, zh) = super::zone_geometry_in(
            &state.layout,
            zone,
            &scene.effective_zone_geometry(zone_def),
            scene.display_area.width,
            scene.display_area.height,
        );
        // A new card is the newest: it takes the first slot.
        let record = ZonePublishRecord {
            publication_id: SceneId::new(),
            zone_name: zone.to_owned(),
            publisher_namespace: String::new(),
            content: ZoneContent::Notification(payload.clone()),
            published_at_wall_us: 0,
            merge_key: None,
            expires_at_wall_us: None,
            content_classification: None,
            breakpoints: Vec::new(),
            lease_id: None,
            publication_origin: None,
        };
        let slots = Compositor::zone_slot_layout_with_tokens(
            &zone_tokens,
            zone,
            std::slice::from_ref(&record),
            policy,
            zh,
            zw,
            &state.cards,
        );
        let Some((_, slot_y, slot_h)) = slots.iter_visible(zy).next() else {
            return Vec::new();
        };
        let mut cards = state.cards.borrow_mut();
        let actions = cards.resolve(&zone_tokens, zone, &record, policy, zw);
        let icon_shown = super::icon::parse_notification_icon(&payload.icon)
            .is_some_and(|id| scene.registered_resources.contains_key(&id));
        let text = notification_text_layout(
            zone,
            tokens,
            &zone_tokens,
            policy,
            payload,
            zw,
            slot_h,
            &actions,
            icon_shown,
            scale,
        );
        let fonts = cards.fallback_fonts(&zone_tokens);
        let mut clipped = Vec::new();
        for band in &text.bands {
            let attrs = Attrs::new()
                .family(family(text.font_family))
                .weight(Weight(band.font_weight.clamp(100, 900)));
            if crate::overflow::truncate_for_ellipsis(
                band.text,
                attrs,
                text.width,
                band.height,
                band.font_size_px,
                band.font_size_px * crate::text::LINE_HEIGHT_MULTIPLIER,
                fonts,
            )
            .was_truncated
            {
                clipped.push(band.field);
            }
        }
        let label = resolve_notification_action_tokens(&zone_tokens);
        let inset = notification_inset(policy, scale);
        let cut_label = payload
            .actions
            .iter()
            .zip(actions.bounds(0.0, slot_y, zw, slot_h, inset))
            .enumerate()
            .any(|(index, (action, bound))| {
                action.label.contains('\n')
                    || crate::text::single_line_width(
                        fonts,
                        &action.label,
                        label.font_size_px,
                        text.font_family,
                        label.font_weight,
                    ) > actions.label_width(&bound, index) + EDGE_SLACK_PX
            });
        if cut_label {
            clipped.push("actions");
        }
        // Drop this probe's cached action layout.
        cards.retain(scene);
        clipped
    }

    fn widget(
        &self,
        scene: &SceneGraph,
        widget: &str,
        params: &HashMap<String, WidgetParameterValue>,
    ) -> Vec<String> {
        let state = self.state();
        if state.tokens.is_none() {
            return Vec::new();
        }
        let Some(def) = scene
            .widget_registry
            .instances
            .get(widget)
            .and_then(|i| scene.widget_registry.definitions.get(&i.widget_type_name))
        else {
            return Vec::new();
        };
        let layers: Vec<(&str, &[WidgetBinding])> = def
            .layers
            .iter()
            .filter_map(|layer| {
                state
                    .widget_svgs
                    .get(&(def.id.clone(), layer.svg_file.clone()))
                    .map(|svg| (svg.as_str(), layer.bindings.as_slice()))
            })
            .collect();
        let constraints: HashMap<String, (f32, f32)> = def
            .parameter_schema
            .iter()
            .filter_map(|p| {
                p.constraints.as_ref().map(|c| {
                    (
                        p.name.clone(),
                        (c.f32_min.unwrap_or(0.0), c.f32_max.unwrap_or(1.0)),
                    )
                })
            })
            .collect();
        crate::widget::clipped_text_params(&layers, &constraints, params)
    }
}

fn family(font: FontFamily) -> Family<'static> {
    match font {
        FontFamily::SystemSansSerif => Family::SansSerif,
        FontFamily::SystemMonospace => Family::Monospace,
        FontFamily::SystemSerif => Family::Serif,
    }
}
