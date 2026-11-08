//! The `portals` block of `/admin/status`: read-only portal UI state, so
//! out-of-process validation (T13) asserts on state instead of inferring it
//! from pixels. Admin-scoped only; never exposed through MCP.
//!
//! Computed when status is requested, from the live scene plus a small
//! main-thread mirror (keyboard focus and draft length), so an idle HUD does
//! no extra work. The draft's text is never reported, only its length.

use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Value, json};
use tze_hud_scene::{
    NodeData, PortalDocumentLayout, PortalPartKind, Rect, SceneGraph, SceneId, ZoneInteractionKind,
};

/// Builds the `portals` array, or `None` while the scene stays locked.
#[derive(Clone)]
pub struct PortalProbe(Arc<dyn Fn() -> Option<Value> + Send + Sync>);

impl PortalProbe {
    pub fn read(&self) -> Option<Value> {
        (self.0)()
    }
}

impl std::fmt::Debug for PortalProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PortalProbe")
    }
}

/// Keyboard focus and draft length as the main thread last saw them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UiMirror {
    /// Focused `(tile, node)`; `node` is `None` for a tile-level stop.
    pub focus: Option<(SceneId, Option<SceneId>)>,
    /// Characters in the focused composer's draft (0 when none is focused).
    pub draft_chars: usize,
}

/// The main thread's [`UiMirror`], written each settle.
pub type UiHandle = Arc<StdMutex<UiMirror>>;

/// The status probe for one runtime: reads `scene` (only the scene lock, as
/// the compositor does, waiting up to 100 ms) and the main thread's `ui`.
pub fn probe(scene: Arc<tokio::sync::Mutex<SceneGraph>>, ui: UiHandle) -> PortalProbe {
    PortalProbe(Arc::new(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        loop {
            if let Ok(scene) = scene.try_lock() {
                let ui = *ui.lock().unwrap_or_else(|e| e.into_inner());
                return Some(portals_json(
                    &scene,
                    ui,
                    &crate::operator::status::display_rects(),
                ));
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }))
}

fn rect_json(r: Rect) -> Value {
    json!({"x": r.x.round(), "y": r.y.round(), "w": r.width.round(), "h": r.height.round()})
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

/// The largest exposed piece of the tile's jump-to-latest pill, from the hit
/// regions the compositor published with its last frame: null exactly when
/// the pill is not drawn (at the tail, faded out, off tab, or covered).
fn unread_badge(scene: &SceneGraph, tile_id: SceneId) -> Option<Rect> {
    scene
        .overlay
        .zone_hit_regions
        .iter()
        .filter(
            |r| matches!(r.kind, ZoneInteractionKind::JumpToLatest { tile_id: t } if t == tile_id),
        )
        .map(|r| r.bounds)
        .max_by(|a, b| (a.width * a.height).total_cmp(&(b.width * b.height)))
}

/// One entry per portal tile, ordered by surface identity.
///
/// `displays` are the overlaid display rects in scene pixels, in
/// `/admin/status` order.
pub fn portals_json(scene: &SceneGraph, ui: UiMirror, displays: &[Rect]) -> Value {
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
                "rect": rect_json(b),
                "display": display,
                // Scene-pixel rects; null when the part is not shown.
                "panes": {
                    "header": part(PortalPartKind::Header),
                    "input": part(PortalPartKind::Composer),
                    "output": part(PortalPartKind::Transcript),
                    "composer": composer,
                    "unread_badge": unread_badge(scene, tile_id).map_or(Value::Null, rect_json),
                },
                "focus": focus,
                "hover": if hovered { "composer" } else { "none" },
                "output": {
                    "first_visible_line": first,
                    "last_visible_line": last,
                    // The follow-tail flag the pill and new-output scrolling obey.
                    "at_tail": scene.tile_follow_tail_at_tail(tile_id),
                    "unread": scene.tile_unread_count(tile_id),
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

    fn portal_scene() -> (SceneGraph, SceneId) {
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
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock));
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
        (scene, tile_id)
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
    fn reports_portal_panes_focus_and_draft_length() {
        let (mut scene, tile_id) = portal_scene();
        let composer = scene.overlay.tile_composer_nodes[&tile_id];
        scene.hit_region_states.get_mut(&composer).unwrap().hovered = true;
        let displays = [Rect::new(0.0, 0.0, 1920.0, 1080.0)];
        let focused = UiMirror {
            focus: Some((tile_id, Some(composer))),
            draft_chars: 5,
        };
        let v = portals_json(&scene, focused, &displays);
        let p = &v[0];
        assert_eq!(p["s"], "portal:probe");
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
        };
        let v = portals_json(&scene, tile_stop, &displays);
        assert_eq!(v[0]["focus"], "tile");
        assert_eq!(v[0]["input"]["draft_chars"], 0);
        let v = portals_json(&scene, UiMirror::default(), &displays);
        assert_eq!(v[0]["focus"], "none");
    }

    #[test]
    fn reports_visible_rows_and_tail_state_as_scrolled() {
        let (mut scene, tile_id) = portal_scene();
        let out =
            |scene: &SceneGraph| portals_json(scene, UiMirror::default(), &[])[0]["output"].clone();
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
        let (mut scene, tile_id) = portal_scene();
        let badge = |scene: &SceneGraph| {
            portals_json(scene, UiMirror::default(), &[])[0]["panes"]["unread_badge"].clone()
        };
        // No pill region published (at tail, hidden or covered): null.
        assert_eq!(badge(&scene), Value::Null);
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
    }
}
