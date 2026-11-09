//! Admin-only surface observations for real-input validation. JSON is built
//! on request from last-frame geometry and a small main-thread mirror, under
//! one bounded scene acquisition. It never wakes or advances the renderer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Value, json};
use tze_hud_compositor::markdown_node_tail_cursor_color;
use tze_hud_scene::{
    NodeData, PortalDocumentLayout, PortalPartKind, Rect, SceneGraph, SceneId, TextMarkdownNode,
    ZoneInteractionKind,
};

/// Reads all admin-only UI blocks together; a busy scene yields null geometry blocks.
#[derive(Clone)]
pub struct SurfaceProbe(Arc<dyn Fn() -> SurfaceSnapshot + Send + Sync>);

#[derive(Default)]
pub struct SurfaceSnapshot {
    pub portals: Value,
    pub notifications: Value,
    pub widgets: Value,
    pub link_launches: u64,
}

impl SurfaceProbe {
    pub fn read(&self) -> SurfaceSnapshot {
        (self.0)()
    }
}

impl std::fmt::Debug for SurfaceProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SurfaceProbe")
    }
}

/// Keyboard focus and draft length as the main thread last saw them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiMirror {
    /// Focused `(tile, node)`; `node` is `None` for a tile-level stop.
    pub focus: Option<(SceneId, Option<SceneId>)>,
    /// Characters in the focused composer's draft (0 when none is focused).
    pub draft_chars: usize,
    /// Actual runtime hover targets, including their configured trigger inset.
    pub hover_targets: HashMap<String, Rect>,
}

/// The main thread's [`UiMirror`], written each settle.
pub type UiHandle = Arc<StdMutex<UiMirror>>;

/// The status probe for one runtime: reads `scene` (only the scene lock, as
/// the compositor does, waiting up to 100 ms) and the main thread's `ui`.
pub fn probe(
    scene: Arc<tokio::sync::Mutex<SceneGraph>>,
    ui: UiHandle,
    link_launches: Arc<AtomicU64>,
) -> SurfaceProbe {
    SurfaceProbe(Arc::new(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        loop {
            if let Ok(scene) = scene.try_lock() {
                let ui = ui.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let displays = crate::operator::status::display_rects();
                return SurfaceSnapshot {
                    portals: portals_json(&scene, &ui, &displays),
                    notifications: notifications_json(&scene, &displays),
                    widgets: widgets_json(&scene, &ui, &displays),
                    link_launches: link_launches.load(Ordering::Relaxed),
                };
            }
            if std::time::Instant::now() >= deadline {
                return SurfaceSnapshot {
                    link_launches: link_launches.load(Ordering::Relaxed),
                    ..Default::default()
                };
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }))
}

fn rect_json(r: Rect) -> Value {
    json!({"x": r.x.round(), "y": r.y.round(), "w": r.width.round(), "h": r.height.round()})
}

fn display_index(bounds: Rect, displays: &[Rect]) -> Option<usize> {
    let (x, y) = (
        bounds.x + bounds.width / 2.0,
        bounds.y + bounds.height / 2.0,
    );
    displays
        .iter()
        .position(|d| x >= d.x && x < d.x + d.width && y >= d.y && y < d.y + d.height)
}

fn notifications_json(scene: &SceneGraph, displays: &[Rect]) -> Value {
    use tze_hud_scene::graph::overlay::NotificationControl;
    let entries: Vec<Value> = scene.overlay.notification_geometry.iter().filter_map(|card| {
        let record = scene.zone_registry.active_publishes.get(&card.zone_name)?
            .iter().find(|r| r.publication_id == card.publication_id)?;
        let tze_hud_scene::ZoneContent::Notification(payload) = &record.content else {
            return None;
        };
        let actions: Vec<Value> = payload.actions.iter().zip(&card.actions).map(|(action, rect)| {
            json!({"label": action.label, "kind": if action.url.is_some() { "url" } else { "callback" }, "rect": rect_json(*rect)})
        }).collect();
        let focus = match card.focus {
            Some(NotificationControl::Action(index)) => json!(index),
            Some(NotificationControl::Close) => json!("close"),
            None => Value::Null,
        };
        Some(json!({
            "s": format!("zone:{}", card.zone_name),
            "key": record.merge_key,
            "rect": rect_json(card.bounds),
            "display": display_index(card.bounds, displays),
            "actions": actions,
            "close": rect_json(card.close),
            "focus": focus,
            "chosen": card.chosen,
            "state": card.state,
        }))
    }).collect();
    json!(entries)
}

fn widgets_json(scene: &SceneGraph, ui: &UiMirror, displays: &[Rect]) -> Value {
    let entries: Vec<Value> = scene.overlay.widget_geometry.iter().filter_map(|geometry| {
        let instance = scene.widget_registry.instances.get(&geometry.instance_name)?;
        let visible = instance.current_params.get("tooltip_visible")
            .is_some_and(|p| matches!(p, tze_hud_scene::WidgetParameterValue::F32(value) if *value >= 0.5));
        Some(json!({
            "s": format!("widget:{}", geometry.instance_name),
            "rect": rect_json(geometry.bounds),
            "display": display_index(geometry.bounds, displays),
            "hover_target": ui.hover_targets.get(&geometry.instance_name).copied().map(rect_json),
            "tooltip_visible": visible,
        }))
    }).collect();
    json!(entries)
}

fn offset(base: Rect, local: Rect) -> Rect {
    Rect::new(
        base.x + local.x,
        base.y + local.y,
        local.width,
        local.height,
    )
}

/// First and last visual rows of `layout` visible at `scroll_y`, as text. A
/// row ends where the next begins, so mixed row heights are measured exactly.
fn visible_rows(layout: &PortalDocumentLayout, scroll_y: f32) -> (Value, Value) {
    let rows = &layout.rows;
    let end = |i: usize| rows.get(i + 1).map_or(layout.content.len(), |r| r.0);
    let bottom_of = |i: usize| rows.get(i + 1).map_or(layout.height, |r| r.1);
    let text = |i: usize| -> Value {
        layout
            .content
            .get(rows[i].0..end(i))
            .map_or(Value::Null, |s| json!(s.trim_end_matches(['\n', '\r'])))
    };
    let bottom = scroll_y + layout.viewport.height;
    let first = (0..rows.len()).find(|&i| bottom_of(i) > scroll_y + 0.5);
    let last = (0..rows.len()).rev().find(|&i| rows[i].1 < bottom - 0.5);
    match (first, last) {
        (Some(f), Some(l)) if f <= l => (text(f), text(l)),
        _ => (Value::Null, Value::Null),
    }
}

/// The layout for `pane`, unless the scene changed since it was measured.
fn current_layout(
    scene: &SceneGraph,
    tile_id: SceneId,
    pane: PortalPartKind,
) -> Option<&PortalDocumentLayout> {
    scene
        .overlay
        .portal_document_layouts
        .get(&(tile_id, pane))
        .filter(|l| scene.portal_document_layout_is_current(tile_id, pane, l))
}

/// Whether `scroll_y` shows the end of `layout`; null until it is measured.
fn at_tail(layout: Option<&PortalDocumentLayout>, scroll_y: f32) -> Value {
    layout.map_or(Value::Null, |l| {
        json!(scroll_y + l.viewport.height >= l.height - 0.5)
    })
}

/// Largest exposed piece of a runtime control in the compositor's last-frame
/// hit regions. A hidden, covered or absent control has no target to report.
fn visible_control(scene: &SceneGraph, kind: ZoneInteractionKind) -> Option<Rect> {
    scene
        .overlay
        .zone_hit_regions
        .iter()
        .filter(|r| r.kind == kind)
        .map(|r| r.bounds)
        .max_by(|a, b| (a.width * a.height).total_cmp(&(b.width * b.height)))
}

/// Read the materialized text node, not a reconstructed Hub status string.
/// Native portals put the title and documents directly under their root.
fn part_text(scene: &SceneGraph, tile: SceneId, kind: PortalPartKind) -> Option<&TextMarkdownNode> {
    let part = scene
        .portal_surface(tile)?
        .parts
        .iter()
        .find(|p| p.kind == kind)?;
    let root = scene.nodes.get(&scene.tiles.get(&tile)?.root_node?)?;
    part.node
        .into_iter()
        .chain(root.children.iter().copied())
        .find_map(|id| {
            let NodeData::TextMarkdown(text) = &scene.nodes.get(&id)?.data else {
                return None;
            };
            (text.bounds.is_within(&part.bounds)
                && (kind == PortalPartKind::Header
                    || scene.portal_document_kind(tile, id) == Some(kind)))
            .then_some(text)
        })
}

/// One entry per portal tile, ordered by surface identity.
///
/// `displays` are the overlaid display rects in scene pixels, in
/// `/admin/status` order.
pub fn portals_json(scene: &SceneGraph, ui: &UiMirror, displays: &[Rect]) -> Value {
    let mut entries: Vec<(String, Value)> = Vec::new();
    for (&tile_id, tile) in &scene.tiles {
        let Some(surface) = scene.portal_surface(tile_id) else {
            continue;
        };
        let b = tile.bounds;
        let part = |kind: PortalPartKind| {
            surface
                .parts
                .iter()
                .find(|p| p.kind == kind)
                .map_or(Value::Null, |p| rect_json(offset(b, p.bounds)))
        };
        let composer_node = scene.overlay.tile_composer_nodes.get(&tile_id).copied();
        let composer = composer_node
            .and_then(|id| match &scene.nodes.get(&id)?.data {
                NodeData::HitRegion(region) => Some(rect_json(offset(b, region.bounds))),
                _ => None,
            })
            .unwrap_or(Value::Null);
        let hovered = composer_node
            .and_then(|id| scene.hit_region_states.get(&id))
            .is_some_and(|s| s.hovered);
        let focus = match ui.focus {
            Some((t, Some(n))) if t == tile_id && Some(n) == composer_node => "composer",
            Some((t, _)) if t == tile_id => "tile",
            _ => "none",
        };
        let (cx, cy) = (b.x + b.width / 2.0, b.y + b.height / 2.0);
        let display = displays
            .iter()
            .position(|d| cx >= d.x && cx < d.x + d.width && cy >= d.y && cy < d.y + d.height);
        let (first, last) = current_layout(scene, tile_id, PortalPartKind::Transcript)
            .map_or((Value::Null, Value::Null), |l| {
                visible_rows(l, scene.tile_scroll_offset_local(tile_id).1)
            });
        let input_y = scene.portal_input_scroll_offset(tile_id).1;
        // `text-stream://portal/<agent>/<id>` is addressed as `portal:<id>`;
        // any other identity is reported as it stands.
        let session = &surface.identity.session_id;
        let s = session
            .strip_prefix("text-stream://portal/")
            .and_then(|rest| rest.split_once('/'))
            .map_or_else(|| session.clone(), |(_, id)| format!("portal:{id}"));
        entries.push((
            session.clone(),
            json!({
                "s": s,
                "state": surface.lifecycle,
                "header_text": part_text(scene, tile_id, PortalPartKind::Header).map(|t| &t.content),
                "rect": rect_json(b),
                "display": display,
                // Scene-pixel rects; null when the part is not shown.
                "panes": {
                    "header": part(PortalPartKind::Header),
                    "input": part(PortalPartKind::Composer),
                    "output": part(PortalPartKind::Transcript),
                    "composer": composer,
                    "unread_badge": visible_control(scene, ZoneInteractionKind::JumpToLatest { tile_id }).map_or(Value::Null, rect_json),
                    "close": visible_control(scene, ZoneInteractionKind::DismissTile { tile_id }).map_or(Value::Null, rect_json),
                },
                "focus": focus,
                "hover": if hovered { "composer" } else { "none" },
                "output": {
                    "first_visible_line": first,
                    "last_visible_line": last,
                    // The follow-tail flag the pill and new-output scrolling obey.
                    "at_tail": scene.tile_follow_tail_at_tail(tile_id),
                    "unread": scene.tile_unread_count(tile_id),
                    "writing_caret": part_text(scene, tile_id, PortalPartKind::Transcript)
                        .is_some_and(|t| markdown_node_tail_cursor_color(t).is_some()),
                },
                "input": {
                    "at_tail": at_tail(current_layout(scene, tile_id, PortalPartKind::Composer), input_y),
                    "draft_chars": if focus == "composer" { ui.draft_chars } else { 0 },
                },
            }),
        ));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Array(entries.into_iter().map(|(_, v)| v).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_portal::hub::{PortalHub, PortalKey, Publish};

    fn portal_scene() -> (
        SceneGraph,
        SceneId,
        crate::portal::PortalDriver,
        tze_hud_scene::TestClock,
    ) {
        let clock = tze_hud_scene::TestClock::from_us(200);
        let mut driver = crate::portal::PortalDriver::new(tze_hud_mcp::PortalHandle::new(
            PortalHub::default(),
            Arc::new(clock.clone()),
        ));
        driver
            .hub_mut()
            .publish(
                &PortalKey::new("agent", "probe"),
                Publish {
                    text: Some("hello".into()),
                    ..Default::default()
                },
                100,
            )
            .expect("publish accepted");
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab = scene.create_tab("Main", 0).unwrap();
        let mut processor = tze_hud_input::InputProcessor::new();
        driver.drain(&mut scene, &mut processor, Some(tab));
        let tile_id = *scene.tiles.keys().next().expect("portal tile");
        // A measured OUTPUT document in the OUTPUT pane: ten rows, 20 px tall
        // except row 2 (40 px), with 60 px visible.
        let (node_id, source) = scene
            .nodes
            .iter()
            .find_map(
                |(id, node)| match (scene.portal_document_kind(tile_id, *id), &node.data) {
                    (Some(PortalPartKind::Transcript), NodeData::TextMarkdown(t)) => {
                        Some((*id, t.clone()))
                    }
                    _ => None,
                },
            )
            .expect("transcript document");
        let pane = scene
            .portal_surface(tile_id)
            .unwrap()
            .parts
            .iter()
            .find(|p| p.kind == PortalPartKind::Transcript)
            .unwrap()
            .bounds;
        let content: String = (0..10).map(|i| format!("l{i}\n")).collect();
        let rows: Vec<(usize, f32)> = (0..10)
            .map(|i| (i * 3, i as f32 * 20.0 + if i > 2 { 20.0 } else { 0.0 }))
            .collect();
        let tile_size = (
            scene.tiles[&tile_id].bounds.width,
            scene.tiles[&tile_id].bounds.height,
        );
        scene.overlay.portal_document_layouts.insert(
            (tile_id, PortalPartKind::Transcript),
            PortalDocumentLayout {
                node_id,
                signature: [2; 32],
                source,
                tile_size,
                font_scale: scene.tile_font_scale(tile_id),
                content: content.into(),
                height: 220.0,
                viewport: Rect::new(pane.x, pane.y, pane.width, 60.0),
                line_height: 20.0,
                rows: rows.into(),
            },
        );
        (scene, tile_id, driver, clock)
    }

    fn jump_region(tile_id: SceneId, bounds: Rect) -> tze_hud_scene::ZoneHitRegion {
        tze_hud_scene::ZoneHitRegion {
            publication_id: SceneId::null(),
            zone_name: "__chrome_jump_to_latest__".into(),
            published_at_wall_us: 0,
            publisher_namespace: "runtime".into(),
            bounds,
            kind: ZoneInteractionKind::JumpToLatest { tile_id },
            interaction_id: format!("jump-to-latest:{tile_id}"),
            tab_order: 0,
        }
    }

    #[test]
    fn reports_last_frame_cards_and_widgets_without_mutating_the_scene() {
        use tze_hud_compositor::renderer::hit_regions::populate_notification_hit_regions;
        use tze_hud_scene::{
            ContentionPolicy, GeometryPolicy, LayerAttachment, NotificationAction,
            NotificationPayload, RenderingPolicy, WidgetInstance, WidgetParameterValue,
            ZoneContent, ZoneDefinition, ZoneMediaType, graph::overlay::WidgetGeometry,
        };
        let (mut scene, _, _, _) = portal_scene();
        let displays = [Rect::new(0.0, 0.0, 1920.0, 1080.0)];
        assert_eq!(notifications_json(&scene, &displays), json!([]));
        assert_eq!(
            widgets_json(&scene, &UiMirror::default(), &displays),
            json!([])
        );
        scene.register_zone(ZoneDefinition {
            id: SceneId::new(),
            name: "notification-area".into(),
            description: String::new(),
            geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.6,
                y_pct: 0.0,
                width_pct: 0.35,
                height_pct: 0.3,
            },
            accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
            rendering_policy: RenderingPolicy::default(),
            contention_policy: ContentionPolicy::Stack { max_depth: 5 },
            max_publishers: 4,
            auto_clear_ms: None,
            layer_attachment: LayerAttachment::Chrome,
            ephemeral: false,
        });
        scene
            .publish_to_zone(
                "notification-area",
                ZoneContent::Notification(NotificationPayload {
                    text: "private body".into(),
                    title: String::new(),
                    icon: String::new(),
                    urgency: 2,
                    ttl_ms: None,
                    actions: vec![
                        NotificationAction {
                            label: "Reply".into(),
                            callback_id: Some("answer".into()),
                            url: None,
                        },
                        NotificationAction {
                            label: "Read".into(),
                            callback_id: None,
                            url: Some("https://example.invalid/?secret=value".into()),
                        },
                    ],
                }),
                "agent",
                Some("card-key".into()),
                None,
                None,
            )
            .unwrap();
        let refresh = |scene: &mut SceneGraph| {
            populate_notification_hit_regions(scene, 1920.0, 1080.0, &HashMap::new());
        };
        refresh(&mut scene);
        let card = notifications_json(&scene, &displays)[0].clone();
        assert_eq!(card["s"], "zone:notification-area");
        assert_eq!(card["key"], "card-key");
        assert_eq!(card["display"], 0);
        assert_eq!(card["actions"][0]["label"], "Reply");
        assert_eq!(card["actions"][0]["kind"], "callback");
        assert_eq!(card["actions"][1]["label"], "Read");
        assert_eq!(card["actions"][1]["kind"], "url");
        assert_eq!(card["actions"].as_array().unwrap().len(), 2);
        assert!(card["focus"].is_null());
        assert!(card["chosen"].is_null());
        assert!(!card.to_string().contains("private body"));
        assert!(!card.to_string().contains("secret=value"));
        for rect in [
            &card["rect"],
            &card["close"],
            &card["actions"][0]["rect"],
            &card["actions"][1]["rect"],
        ] {
            let r = Rect::new(
                rect["x"].as_f64().unwrap() as f32,
                rect["y"].as_f64().unwrap() as f32,
                rect["w"].as_f64().unwrap() as f32,
                rect["h"].as_f64().unwrap() as f32,
            );
            assert!(
                r.width > 0.0 && r.height > 0.0 && r.is_within(&displays[0]),
                "{rect}"
            );
        }
        let action = scene
            .overlay
            .zone_hit_regions
            .iter()
            .find(|r| matches!(r.kind, ZoneInteractionKind::Action { index: 1 }))
            .unwrap()
            .clone();
        scene.overlay.notification_focus = Some(action.interaction_id.clone());
        refresh(&mut scene);
        assert_eq!(notifications_json(&scene, &displays)[0]["focus"], 1);
        let close = scene
            .overlay
            .zone_hit_regions
            .iter()
            .find(|r| matches!(r.kind, ZoneInteractionKind::Dismiss))
            .unwrap()
            .clone();
        scene.overlay.notification_focus = Some(close.interaction_id);
        refresh(&mut scene);
        assert_eq!(notifications_json(&scene, &displays)[0]["focus"], "close");
        scene.overlay.notification_focus = None;
        scene
            .overlay
            .notification_choices
            .insert(action.publication_id, action.interaction_id);
        refresh(&mut scene);
        assert!(
            scene.overlay.zone_hit_regions.is_empty(),
            "chosen card takes no input"
        );
        for state in ["entering", "shown", "exiting"] {
            scene.overlay.notification_geometry[0].state = state;
            let v = notifications_json(&scene, &displays);
            assert_eq!(
                v[0]["rect"], card["rect"],
                "retain exit geometry without hit regions"
            );
            assert_eq!(v[0]["state"], state);
            assert_eq!(v[0]["chosen"], 1);
            assert!(v[0]["focus"].is_null());
        }
        // A replacement record must never inherit old bounds under new labels.
        scene
            .zone_registry
            .active_publishes
            .get_mut("notification-area")
            .unwrap()[0]
            .publication_id = SceneId::new();
        assert_eq!(notifications_json(&scene, &displays), json!([]));
        scene.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "test".into(),
            tab_id: scene.active_tab.unwrap(),
            geometry_override: None,
            contention_override: None,
            instance_name: "test".into(),
            current_params: HashMap::new(),
            auto_clear_ms: None,
        });
        assert_eq!(
            widgets_json(&scene, &UiMirror::default(), &displays),
            json!([]),
            "registry instance without a drawn quad"
        );
        scene.overlay.widget_geometry.push(WidgetGeometry {
            instance_name: "test".into(),
            bounds: Rect::new(40.0, 20.0, 320.0, 100.0),
        });
        let mut ui = UiMirror::default();
        for (value, visible) in [(0.0, false), (0.499, false), (0.5, true), (1.0, true)] {
            scene
                .widget_registry
                .instances
                .get_mut("test")
                .unwrap()
                .current_params
                .insert("tooltip_visible".into(), WidgetParameterValue::F32(value));
            let v = widgets_json(&scene, &ui, &displays);
            assert_eq!(v[0]["s"], "widget:test");
            assert_eq!(v[0]["display"], 0);
            assert_eq!(v[0]["rect"], json!({"x":40.0,"y":20.0,"w":320.0,"h":100.0}));
            assert!(v[0]["hover_target"].is_null());
            assert_eq!(v[0]["tooltip_visible"], visible);
        }
        let target = Rect::new(320.0, 20.0, 40.0, 32.0);
        ui.hover_targets.insert("test".into(), target);
        assert_eq!(
            widgets_json(&scene, &ui, &displays)[0]["hover_target"],
            rect_json(target)
        );
        let version = scene.version;
        let scene = Arc::new(tokio::sync::Mutex::new(scene));
        let launches = Arc::new(AtomicU64::new(7));
        let observation = probe(scene.clone(), Arc::new(StdMutex::new(ui)), launches);
        for _ in 0..3 {
            assert_eq!(observation.read().widgets.as_array().unwrap().len(), 1);
            assert_eq!(scene.try_lock().unwrap().version, version);
        }
        let _held = scene.try_lock().unwrap();
        let started = std::time::Instant::now();
        let unavailable = observation.read();
        assert!(
            unavailable.portals.is_null()
                && unavailable.notifications.is_null()
                && unavailable.widgets.is_null()
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "one 100ms acquisition for all blocks"
        );
        assert_eq!(
            unavailable.link_launches, 7,
            "scene contention must not reset the monotonic counter"
        );
    }

    #[test]
    fn reports_portal_panes_focus_and_draft_length() {
        let (mut scene, tile_id, mut driver, clock) = portal_scene();
        let composer = scene.overlay.tile_composer_nodes[&tile_id];
        scene.hit_region_states.get_mut(&composer).unwrap().hovered = true;
        let displays = [Rect::new(0.0, 0.0, 1920.0, 1080.0)];
        let focused = UiMirror {
            focus: Some((tile_id, Some(composer))),
            draft_chars: 5,
            ..Default::default()
        };
        let v = portals_json(&scene, &focused, &displays);
        let p = &v[0];
        assert_eq!(p["s"], "portal:probe");
        assert!(p["header_text"].as_str().unwrap().contains("⋯ writing"));
        assert_eq!(p["output"]["writing_caret"], true);
        assert_eq!(p["display"], 0);
        for pane in ["header", "input", "output", "composer"] {
            assert!(p["panes"][pane]["w"].as_f64().unwrap() > 0.0, "{pane} rect");
        }
        assert_eq!(p["focus"], "composer");
        assert_eq!(p["hover"], "composer");
        assert_eq!(p["input"]["draft_chars"], 5);
        assert_eq!(p["input"]["at_tail"], Value::Null, "INPUT not measured yet");

        // A tile-level stop is not composer focus, and hides the draft length.
        let tile_stop = UiMirror {
            focus: Some((tile_id, None)),
            draft_chars: 5,
            ..Default::default()
        };
        let v = portals_json(&scene, &tile_stop, &displays);
        assert_eq!(v[0]["focus"], "tile");
        assert_eq!(v[0]["input"]["draft_chars"], 0);
        let v = portals_json(&scene, &UiMirror::default(), &displays);
        assert_eq!(v[0]["focus"], "none");

        // Observe the actual driver clearing both rendered cues, without a
        // replacement publish or reconstructed Hub deadline in the probe.
        clock.advance(2001);
        let tab = scene.active_tab;
        driver.drain(&mut scene, &mut tze_hud_input::InputProcessor::new(), tab);
        let v = portals_json(&scene, &UiMirror::default(), &displays);
        assert!(!v[0]["header_text"].as_str().unwrap().contains("⋯ writing"));
        assert_eq!(v[0]["output"]["writing_caret"], false);
    }

    #[test]
    fn reports_visible_rows_and_tail_state_as_scrolled() {
        let (mut scene, tile_id, _, _) = portal_scene();
        let out = |scene: &SceneGraph| {
            portals_json(scene, &UiMirror::default(), &[])[0]["output"].clone()
        };
        let rows = |scene: &SceneGraph| {
            let o = out(scene);
            (
                o["first_visible_line"].clone(),
                o["last_visible_line"].clone(),
            )
        };
        // Row 2 spans 40..80, so it stays first until the viewport passes 80.
        for (y, first, last) in [(0.0, "l0", "l2"), (19.6, "l1", "l2"), (65.0, "l2", "l5")] {
            scene.set_tile_scroll_offset_local(tile_id, 0.0, y).unwrap();
            assert_eq!(rows(&scene), (json!(first), json!(last)), "scroll {y}");
        }
        scene.set_tile_follow_tail_at_tail(tile_id, true);
        assert_eq!(out(&scene)["at_tail"], true);
        scene.set_tile_follow_tail_at_tail(tile_id, false);
        assert_eq!(out(&scene)["at_tail"], false);
        assert_eq!(out(&scene)["unread"], scene.tile_unread_count(tile_id));

        // A layout measured for another tile size is stale: no rows reported.
        let key = (tile_id, PortalPartKind::Transcript);
        scene
            .overlay
            .portal_document_layouts
            .get_mut(&key)
            .unwrap()
            .tile_size
            .0 += 1.0;
        assert_eq!(rows(&scene), (Value::Null, Value::Null));
    }

    #[test]
    fn unread_badge_is_the_largest_exposed_pill_piece() {
        let (mut scene, tile_id, _, _) = portal_scene();
        let badge = |scene: &SceneGraph| {
            portals_json(scene, &UiMirror::default(), &[])[0]["panes"]["unread_badge"].clone()
        };
        // No pill region published (at tail, hidden or covered): null.
        assert_eq!(badge(&scene), Value::Null);
        assert_eq!(
            portals_json(&scene, &UiMirror::default(), &[])[0]["panes"]["close"],
            Value::Null
        );
        let other = SceneId::new();
        scene.overlay.zone_hit_regions.extend([
            jump_region(tile_id, Rect::new(10.0, 20.0, 30.0, 24.0)),
            jump_region(tile_id, Rect::new(40.0, 20.0, 66.0, 24.0)),
            jump_region(other, Rect::new(0.0, 0.0, 500.0, 500.0)),
        ]);
        assert_eq!(
            badge(&scene),
            json!({"x": 40.0, "y": 20.0, "w": 66.0, "h": 24.0})
        );
        // A close target is reported only from the compositor's exposed hit
        // regions, never guessed from a tile corner or another tile's button.
        let mut close = jump_region(tile_id, Rect::new(80.0, 20.0, 24.0, 24.0));
        close.kind = ZoneInteractionKind::DismissTile { tile_id };
        let mut foreign = close.clone();
        foreign.kind = ZoneInteractionKind::DismissTile { tile_id: other };
        foreign.bounds.width = 500.0;
        scene.overlay.zone_hit_regions.extend([close, foreign]);
        let v = portals_json(&scene, &UiMirror::default(), &[]);
        assert_eq!(
            v[0]["panes"]["close"],
            json!({"x": 80.0, "y": 20.0, "w": 24.0, "h": 24.0})
        );
        scene.overlay.zone_hit_regions.clear();
        assert_eq!(
            portals_json(&scene, &UiMirror::default(), &[])[0]["panes"]["close"],
            Value::Null
        );
    }
}
