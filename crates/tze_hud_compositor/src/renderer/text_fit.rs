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
    ContentionPolicy, FontFamily, NotificationPayload, Rect, StatusBarPayload, WidgetBinding,
    WidgetParameterValue, ZoneContent, ZonePublishRecord,
};

use super::notification_layout::NotificationLayoutCache;
use super::notification_text::notification_text_layout;
use super::{Compositor, ZoneStack};
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
    /// The profile's `max_truncation_input_bytes`; `None` keeps the default.
    max_input_bytes: Option<usize>,
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

    /// The text byte budget the status bar measures within.
    pub fn set_max_truncation_input_bytes(&self, bytes: usize) {
        self.state().max_input_bytes = Some(bytes);
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
        display: Option<&str>,
    ) -> Vec<(&'static str, usize)> {
        let state = self.state();
        let Some(tokens) = &state.tokens else {
            return Vec::new();
        };
        let Some((zone_name, zone_def)) = scene.zone_registry.zones.get_key_value(zone) else {
            return Vec::new();
        };
        // Only Stack zones draw notification cards.
        if !matches!(zone_def.contention_policy, ContentionPolicy::Stack { .. }) {
            return Vec::new();
        }
        let policy = &zone_def.rendering_policy;
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
            caption: None,
            window_token: None,
            display: None,
        };
        let stack = ZoneStack {
            name: zone_name,
            publishes: std::borrow::Cow::Borrowed(std::slice::from_ref(&record)),
            display: ZoneStack::placed_display(
                &state.layout,
                ZoneStack::zone_display(&state.layout, zone),
                display,
            ),
        };
        let zone_tokens = tze_hud_config::tokens::for_zone(tokens, &stack.token_key());
        let (zx, zy, zw, zh) = super::stack_geometry_in(
            &state.layout,
            &stack,
            &scene.effective_zone_geometry(zone_def),
            scene.display_area.width,
            scene.display_area.height,
        );
        let slots = Compositor::zone_slot_layout_with_tokens(
            &zone_tokens,
            scene,
            zone,
            &stack.publishes,
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
        let text = notification_text_layout(
            zone,
            &zone_tokens,
            policy,
            payload,
            // A session caption only adds a line above: the column width,
            // and so every count below, is the same with or without it.
            None,
            card.width,
            agent_icon,
        );
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

    fn status_bar(
        &self,
        scene: &SceneGraph,
        zone: &str,
        payload: &StatusBarPayload,
        merge_key: Option<&str>,
    ) -> Vec<(String, usize)> {
        let state = self.state();
        let Some(tokens) = &state.tokens else {
            return Vec::new();
        };
        let Some(zone_def) = scene.zone_registry.zones.get(zone) else {
            return Vec::new();
        };
        // The bar as merged right after this publish, as the scene applies it.
        let mut records = scene
            .zone_registry
            .active_publishes
            .get(zone)
            .cloned()
            .unwrap_or_default();
        let record = ZonePublishRecord {
            publication_id: SceneId::new(),
            zone_name: zone.to_owned(),
            publisher_namespace: String::new(),
            content: ZoneContent::StatusBar(payload.clone()),
            published_at_wall_us: 0,
            merge_key: merge_key.map(str::to_owned),
            expires_at_wall_us: None,
            content_classification: None,
            breakpoints: Vec::new(),
            lease_id: None,
            publication_origin: None,
            caption: None,
            window_token: None,
            display: None,
        };
        // A publish the scene would refuse has nothing to report.
        if tze_hud_scene::graph::apply_contention(
            &mut records,
            record,
            zone_def.contention_policy,
            zone_def.max_publishers,
            |_| tze_hud_scene::ValidationError::ZoneMaxPublishersReached {
                zone: zone.to_owned(),
                max: zone_def.max_publishers,
            },
            |_| false,
        )
        .is_err()
        {
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
        let display = state.layout.zone_display_rect(zone).unwrap_or_else(|| {
            Rect::new(
                0.0,
                0.0,
                scene.display_area.width,
                scene.display_area.height,
            )
        });
        let mut cards = state.cards.borrow_mut();
        super::status_bar::clipped_values(
            &records,
            zone_def.contention_policy,
            &payload.entries,
            &policy.key_icon_map,
            super::status_bar::StatusStyle::resolve(&zone_tokens, policy),
            Rect::new(zx, zy, zw, zh),
            display,
            cards.fallback_fonts(&zone_tokens),
            state
                .max_input_bytes
                .unwrap_or(crate::overflow::DEFAULT_MAX_TRUNCATION_INPUT_BYTES),
        )
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
                    role: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn reported(clipped: &[(&'static str, usize)], field: &str) -> Option<usize> {
        clipped.iter().find(|(f, _)| *f == field).map(|(_, n)| *n)
    }

    #[test]
    fn placed_notification_fit_uses_drawing_bounds_and_scale() {
        use crate::display::{DisplayRect, normalize_display_name, placed_zone_key};
        use tze_hud_config::tokens::set_zone_px_scale;

        let (fit, scene) = measurer_and_scene();
        let primary = DisplayRect {
            name: "DISPLAY1".into(),
            rect: Rect::new(0.0, 0.0, 1280.0, 720.0),
            primary: true,
        };
        let secondary = DisplayRect {
            name: "DISPLAY2".into(),
            rect: Rect::new(-768.0, 0.0, 768.0, 720.0),
            primary: false,
        };
        let assignments = HashMap::new();
        fit.set_display_layout(DisplayLayout::new(
            vec![primary.clone(), secondary],
            &assignments,
        ));
        let title = "The nightly migration is ready for review across every shard";
        let card = card(title, "", &["Review", "Later"]);
        let primary_fit = fit.notification(&scene, ZONE, &card, None);
        let placed = || fit.notification(&scene, ZONE, &card, Some("display2"));
        let display_width_fit = placed();
        let title_count =
            |fit: &[(&'static str, usize)]| reported(fit, "title").unwrap_or(title.chars().count());
        assert!(title_count(&display_width_fit) < title_count(&primary_fit));

        let mut tokens = fit.state().tokens.clone().unwrap();
        let display = normalize_display_name("DISPLAY2");
        set_zone_px_scale(&mut tokens, &placed_zone_key(ZONE, &display), 1.5);
        fit.set_tokens(tokens);
        let scaled_fit = placed();
        assert!(title_count(&scaled_fit) < title_count(&display_width_fit));
        assert_eq!(fit.notification(&scene, ZONE, &card, None), primary_fit);

        fit.set_display_layout(DisplayLayout::new(vec![primary], &assignments));
        assert_eq!(placed(), primary_fit);
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
        let clipped = fit.notification(&scene, ZONE, &card(title, body, &[]), None);
        let t = reported(&clipped, "title").expect("title cut");
        let b = reported(&clipped, "body").expect("body cut");
        assert!(t > 0 && t < title.chars().count(), "{clipped:?}");
        assert!(b > 0 && b < body.chars().count(), "{clipped:?}");
        let again = fit.notification(
            &scene,
            ZONE,
            &card(char_prefix(title, t), char_prefix(body, b), &[]),
            None,
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
        let clipped = fit.notification(&scene, ZONE, &card("", "Deploy?", &labels), None);
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
                &fit.notification(
                    &scene,
                    ZONE,
                    &card("", "Deploy?", &[label, "Hold", "Ok"]),
                    None,
                ),
                "actions",
            )
        };
        let (a, b) = (alone(long).unwrap(), alone(longer).unwrap());
        assert!(a > 0 && b > 0);
        assert_eq!(n, a.min(b), "smallest: {n} vs {a}/{b}");
        let shortened: Vec<&str> = labels.iter().map(|l| char_prefix(l, n)).collect();
        let again = fit.notification(&scene, ZONE, &card("", "Deploy?", &shortened), None);
        assert!(again.is_empty(), "{n}: {again:?}");
        let one_more = [char_prefix(long, a + 1), "Hold", "Ok"];
        assert_eq!(
            reported(
                &fit.notification(&scene, ZONE, &card("", "Deploy?", &one_more), None),
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
            None,
        );
        assert_eq!(clipped, [("actions", 5)]);

        let line = "Deploy the release to every production region and every staging region";
        let label = format!("{line}\nthen page");
        let clipped = fit.notification(&scene, ZONE, &card("", "Deploy?", &[&label, "No"]), None);
        let n = reported(&clipped, "actions").expect("cut");
        assert!(n > 0 && n < line.chars().count(), "{clipped:?}");
        let again = fit.notification(
            &scene,
            ZONE,
            &card("", "Deploy?", &[char_prefix(line, n), "No"]),
            None,
        );
        assert!(again.is_empty(), "{n}: {again:?}");
    }

    /// Republishing every label at the reported `actions` count fits, also
    /// when a link action's mark decides whether the buttons sit side by side.
    #[test]
    fn link_labels_at_the_reported_count_fit() {
        let (fit, scene) = measurer_and_scene();
        let payload = |first: &str, second: &str| NotificationPayload {
            text: "Deploy?".into(),
            actions: vec![
                NotificationAction {
                    label: first.into(),
                    callback_id: Some("go".into()),
                    url: None,
                    role: None,
                },
                NotificationAction {
                    label: second.into(),
                    callback_id: None,
                    url: Some("https://example.com/".into()),
                    role: None,
                },
            ],
            ..Default::default()
        };
        // A line break forces stacked buttons and stops the count at k; the
        // shortened labels may then sit side by side, where the link's mark
        // narrows its button.
        // Sweeping k walks each link prefix across the side-by-side limit.
        for link in [
            "Documentation and the full deployment guide",
            "WWW MMM WWW MMM WWW MMM WWW MMM WWW",
        ] {
            for k in 1..=30 {
                let first = format!("{}\nmore", "i".repeat(k));
                let clipped = fit.notification(&scene, ZONE, &payload(&first, link), None);
                let n = reported(&clipped, "actions").expect("line break cuts");
                let again = fit.notification(
                    &scene,
                    ZONE,
                    &payload(char_prefix(&first, n), char_prefix(link, n)),
                    None,
                );
                assert!(again.is_empty(), "{link} k={k} n={n}: {again:?}");
            }
        }
    }

    /// Text that fits reports nothing.
    #[test]
    fn a_card_that_fits_reports_nothing() {
        let (fit, scene) = measurer_and_scene();
        let clipped = fit.notification(
            &scene,
            ZONE,
            &card("Build", "All tests passed.", &["Open", "Ok"]),
            None,
        );
        assert!(clipped.is_empty(), "{clipped:?}");
    }

    fn bar_scene() -> (TextFitMeasurer, SceneGraph) {
        let (fit, mut scene) = measurer_and_scene();
        scene.register_zone(ZoneRegistry::with_defaults().zones["status-bar"].clone());
        (fit, scene)
    }

    fn status(entries: &[(&str, &str)]) -> StatusBarPayload {
        StatusBarPayload {
            entries: entries
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    /// R2.2 S1: a cut status value reports the longest prefix its entry
    /// draws whole, within the per-field layout bound.
    #[test]
    fn cut_status_value_counts_what_republishes_whole() {
        let (fit, scene) = bar_scene();
        let value = "deploying release candidate 42 to every production and staging region";
        let measure = |v: &str| {
            fit.status_bar(
                &scene,
                "status-bar",
                &status(&[("build", v)]),
                Some("build"),
            )
        };
        PREFIX_PROBES.with(|p| p.set(0));
        let clipped = measure(value);
        let probes = PREFIX_PROBES.with(std::cell::Cell::get);
        assert!(probes <= bound(value.chars().count()), "{probes} layouts");
        let [(key, n)] = clipped.as_slice() else {
            panic!("one cut entry: {clipped:?}");
        };
        assert_eq!(key, "build");
        assert!(*n > 0 && *n < value.chars().count(), "{clipped:?}");
        assert!(measure(char_prefix(value, *n)).is_empty(), "{n} fits");
        assert_eq!(
            measure(char_prefix(value, n + 1)),
            [("build".to_owned(), *n)],
            "one more character is cut"
        );
        assert!(measure("ok").is_empty(), "a value that fits");
        assert!(measure("").is_empty(), "a removal");
    }

    /// R2.2 S2: an entry the full bar does not draw reports 0.
    #[test]
    fn status_entry_past_a_full_bar_counts_zero() {
        let (fit, scene) = bar_scene();
        let entries: Vec<(String, String)> = (0..12)
            .map(|i| {
                (
                    format!("k{i:02}"),
                    format!("value number {i} with plenty of words"),
                )
            })
            .collect();
        let pairs: Vec<(&str, &str)> = entries
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let clipped = fit.status_bar(&scene, "status-bar", &status(&pairs), None);
        assert_eq!(
            clipped.last(),
            Some(&("k11".to_owned(), 0)),
            "the last entry is not drawn: {clipped:?}"
        );
        // Entries that fit report nothing; the last drawn one is cut, and
        // every entry after it is not drawn.
        let first_zero = clipped.iter().position(|(_, n)| *n == 0).expect("undrawn");
        assert!(
            first_zero > 0 && clipped[first_zero - 1].1 > 0,
            "{clipped:?}"
        );
        assert!(
            clipped[first_zero..].iter().all(|(_, n)| *n == 0),
            "{clipped:?}"
        );
    }

    /// The bar is merged with the scene's own contention rules: a Stack
    /// status bar at full depth evicts its oldest record and draws only the
    /// newest, and a zero-depth stack discards the publish, so nothing it
    /// set is measured.
    #[test]
    fn status_fit_applies_the_scene_contention_rules() {
        let (fit, mut scene) = bar_scene();
        let value = "deploying release candidate 42 to every production and staging region";
        let alone = fit.status_bar(&scene, "status-bar", &status(&[("build", value)]), None);
        assert_eq!(alone.len(), 1, "{alone:?}");
        let old = |key: &str| ZonePublishRecord {
            publication_id: SceneId::new(),
            zone_name: "status-bar".into(),
            publisher_namespace: String::new(),
            content: ZoneContent::StatusBar(status(&[(key, value)])),
            published_at_wall_us: 0,
            merge_key: None,
            expires_at_wall_us: None,
            content_classification: None,
            breakpoints: Vec::new(),
            lease_id: None,
            publication_origin: None,
            caption: None,
            window_token: None,
            display: None,
        };
        scene
            .zone_registry
            .active_publishes
            .insert("status-bar".into(), vec![old("a"), old("b")]);
        let zone = scene.zone_registry.zones.get_mut("status-bar").unwrap();
        zone.contention_policy = ContentionPolicy::Stack { max_depth: 2 };
        zone.max_publishers = 8;
        let stacked = fit.status_bar(&scene, "status-bar", &status(&[("build", value)]), None);
        assert_eq!(stacked, alone, "only the newest record draws");
        scene
            .zone_registry
            .zones
            .get_mut("status-bar")
            .unwrap()
            .contention_policy = ContentionPolicy::Stack { max_depth: 0 };
        let discarded = fit.status_bar(&scene, "status-bar", &status(&[("build", value)]), None);
        assert!(discarded.is_empty(), "{discarded:?}");
    }
}
