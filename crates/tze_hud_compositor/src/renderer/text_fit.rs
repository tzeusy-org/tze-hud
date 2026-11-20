//! Admission-time text fit (`docs/api.md` "clipped").
//!
//! The compositor keeps a [`TextFitMeasurer`] in step with what it draws
//! with: the token map, the display layout, and the token-resolved widget
//! SVGs. MCP admission asks it which published text would be cut, and how
//! many characters of each cut field are drawn, using the same card geometry ([`super::notification_text`], the slot layout and the
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
    ) -> Vec<(&'static str, usize)> {
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
        let policy = &zone_def.rendering_policy;
        let (zx, zy, zw, zh) = super::zone_geometry_in(
            &state.layout,
            zone,
            &scene.effective_zone_geometry(zone_def),
            scene.display_area.width,
            scene.display_area.height,
        );
        // Measure intrinsic text fit in one card; stack visibility is a
        // separate question from whether its text fits inside that card.
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
            scene,
            zone,
            std::slice::from_ref(&record),
            policy,
            (zw, zh),
            &state.cards,
        );
        let Some((_, card)) = slots
            .iter_visible(tze_hud_scene::types::Rect::new(zx, zy, zw, zh))
            .next()
        else {
            return Vec::new();
        };
        let mut cards = state.cards.borrow_mut();
        let actions = cards.resolve(&zone_tokens, zone, &record, policy, card.width);
        let agent_icon = super::icon::parse_notification_icon(&payload.icon)
            .is_some_and(|id| scene.registered_resources.contains_key(&id));
        let text =
            notification_text_layout(zone, &zone_tokens, policy, payload, card.width, agent_icon);
        let fonts = cards.fallback_fonts(&zone_tokens);
        let text_family = text.font_family;
        let mut clipped = Vec::new();
        for band in &text.bands {
            let attrs = Attrs::new()
                .family(family(text.font_family))
                .weight(Weight(band.font_weight.clamp(100, 900)));
            let result = crate::overflow::truncate_for_ellipsis(
                band.text,
                attrs,
                text.width,
                band.height,
                band.font_size_px,
                band.font_size_px
                    * band
                        .line_multiplier
                        .unwrap_or(crate::text::LINE_HEIGHT_MULTIPLIER),
                fonts,
            );
            if result.was_truncated {
                clipped.push((band.field, result.drawn_chars(band.text)));
            }
        }
        let style =
            super::notification_style::NotificationStyle::for_zone(zone, &zone_tokens, policy);
        let label = (style.action_label.size, style.action_label.weight);
        let mut fits = |text: &str, room: f32| {
            crate::text::single_line_width(fonts, text, label.0, text_family, label.1)
                <= room + EDGE_SLACK_PX
        };
        // The smallest drawn count among the cut labels: a label that fits
        // is measured once; a cut one is searched for its longest fitting
        // prefix, which stops before a line break.
        let cut_label = payload
            .actions
            .iter()
            .zip(actions.bounds(card.x, card.y, card.width, card.height))
            .enumerate()
            .filter_map(|(index, (action, bound))| {
                let room = actions.label_width(&bound, index);
                let line = action.label.split('\n').next().unwrap_or_default();
                let broken = line.len() < action.label.len();
                if !broken && fits(line, room) {
                    return None;
                }
                let line_chars = line.chars().count();
                // An unbroken label is known not to fit whole.
                let upper = if broken {
                    line_chars
                } else {
                    line_chars.saturating_sub(1)
                };
                Some(crate::overflow::longest_fitting_prefix(upper, |n| {
                    fits(crate::overflow::char_prefix(line, n), room)
                }))
            })
            .min();
        if let Some(count) = cut_label {
            clipped.push(("actions", count));
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
    ) -> Vec<(String, usize)> {
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

#[cfg(test)]
mod tests {
    //! Drawn counts (`clipped`) from the real card shapers.
    use super::*;
    use crate::overflow::{PREFIX_PROBES, char_prefix};
    use tze_hud_scene::TextFit;
    use tze_hud_scene::types::{NotificationAction, ZoneRegistry};

    const ZONE: &str = "notification-area";

    fn measurer_and_scene() -> (TextFitMeasurer, SceneGraph) {
        let fit = TextFitMeasurer::new();
        fit.set_tokens(HashMap::from([
            ("font.sans".into(), "IBM Plex Sans".into()),
            ("font.mono".into(), "IBM Plex Mono".into()),
        ]));
        let mut scene = SceneGraph::new(1280.0, 720.0);
        let defaults = ZoneRegistry::with_defaults();
        scene.register_zone(defaults.zones[ZONE].clone());
        (fit, scene)
    }

    fn card(title: &str, body: &str, labels: &[&str]) -> NotificationPayload {
        NotificationPayload {
            title: title.into(),
            text: body.into(),
            actions: labels
                .iter()
                .enumerate()
                .map(|(i, label)| NotificationAction {
                    label: (*label).into(),
                    callback_id: Some(format!("a{i}")),
                    url: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn reported(clipped: &[(&'static str, usize)], field: &str) -> Option<usize> {
        clipped.iter().find(|(f, _)| *f == field).map(|(_, n)| *n)
    }

    /// ceil(log2(len)) + 1: the per-field layout bound.
    fn bound(len: usize) -> usize {
        (usize::BITS - len.saturating_sub(1).leading_zeros()) as usize + 1
    }

    /// Title and body report the characters drawn before the ellipsis; that
    /// many characters, republished, fit.
    #[test]
    fn title_and_body_counts_republish_uncut() {
        let (fit, scene) = measurer_and_scene();
        let title = "Nightly migration report for every production shard and region";
        let body = "The nightly migration finished but three shards reported checksum \
            drift that needs a manual look before tomorrow's deploy window opens";
        let clipped = fit.notification(&scene, ZONE, &card(title, body, &[]));
        let t = reported(&clipped, "title").expect("title cut");
        let b = reported(&clipped, "body").expect("body cut");
        assert!(t > 0 && t < title.chars().count(), "{clipped:?}");
        assert!(b > 0 && b < body.chars().count(), "{clipped:?}");
        let again = fit.notification(
            &scene,
            ZONE,
            &card(char_prefix(title, t), char_prefix(body, b), &[]),
        );
        assert!(again.is_empty(), "{t}/{b}: {again:?}");
    }

    /// `actions` is the smallest count among the cut labels, each found
    /// within the per-field layout bound; shortening every label to it fits.
    #[test]
    fn actions_report_the_smallest_cut_label() {
        let (fit, scene) = measurer_and_scene();
        let long =
            "Deploy the release to every production region and every staging region right now";
        let longer =
            "Roll back every region, then page the on-call engineer for a full incident review";
        let labels = [long, "Hold", longer];
        PREFIX_PROBES.with(|p| p.set(0));
        let clipped = fit.notification(&scene, ZONE, &card("", "Deploy?", &labels));
        let probes = PREFIX_PROBES.with(std::cell::Cell::get);
        assert!(
            probes <= bound(long.chars().count()) + bound(longer.chars().count()),
            "{probes} layouts"
        );
        let n = reported(&clipped, "actions").expect("labels cut");
        assert_eq!(clipped, [("actions", n)]);
        // Each cut label alone reports its own count; the reply carries the
        // smaller one.
        let alone = |label: &str| {
            reported(
                &fit.notification(&scene, ZONE, &card("", "Deploy?", &[label, "Hold", "Ok"])),
                "actions",
            )
        };
        let (a, b) = (alone(long).unwrap(), alone(longer).unwrap());
        assert!(a > 0 && b > 0);
        let shortened: Vec<&str> = labels.iter().map(|l| char_prefix(l, n)).collect();
        let again = fit.notification(&scene, ZONE, &card("", "Deploy?", &shortened));
        assert!(again.is_empty(), "{n}: {again:?}");
        let one_more = [char_prefix(long, a + 1), "Hold", "Ok"];
        assert_eq!(
            reported(
                &fit.notification(&scene, ZONE, &card("", "Deploy?", &one_more)),
                "actions"
            ),
            Some(a),
            "one more character is cut"
        );
    }

    /// A label with a line break counts up to the break when that fits, and
    /// to its fitting prefix otherwise.
    #[test]
    fn a_line_break_ends_the_label_count() {
        let (fit, scene) = measurer_and_scene();
        let clipped = fit.notification(
            &scene,
            ZONE,
            &card("", "Retry?", &["Retry\nwith detail", "No"]),
        );
        assert_eq!(clipped, [("actions", 5)]);

        let line = "Deploy the release to every production region and every staging region";
        let label = format!("{line}\nthen page");
        let clipped = fit.notification(&scene, ZONE, &card("", "Deploy?", &[&label, "No"]));
        let n = reported(&clipped, "actions").expect("cut");
        assert!(n > 0 && n < line.chars().count(), "{clipped:?}");
        let again = fit.notification(
            &scene,
            ZONE,
            &card("", "Deploy?", &[char_prefix(line, n), "No"]),
        );
        assert!(again.is_empty(), "{n}: {again:?}");
    }

    /// Text that fits reports nothing.
    #[test]
    fn a_card_that_fits_reports_nothing() {
        let (fit, scene) = measurer_and_scene();
        let clipped = fit.notification(
            &scene,
            ZONE,
            &card("Build", "All tests passed.", &["Open", "Ok"]),
        );
        assert!(clipped.is_empty(), "{clipped:?}");
    }
}
