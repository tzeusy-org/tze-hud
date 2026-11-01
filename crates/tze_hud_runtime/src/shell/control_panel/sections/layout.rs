//! Layout: where each widget sits and how long its content stays up, both
//! live and saved to its `[[tabs.widgets]]` entry, and which zones a
//! `[displays.<NAME>]` table sends to another monitor (read-only until
//! monitor names are stable).

use std::sync::Arc;

use tokio::sync::Mutex;
use tze_hud_config::WidgetUpdate;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::render_wake::RenderWakeNotifier;
use tze_hud_scene::types::GeometryPolicy;

use super::super::row::{BoxFuture, Control, Edit, Row, Section};
use super::super::runtime::WidgetWriter;

/// "Clears after" choices, in ms (`None`: never).
const CLEAR_CHOICES: [Option<u64>; 8] = [
    None,
    Some(5_000),
    Some(10_000),
    Some(30_000),
    Some(60_000),
    Some(300_000),
    Some(900_000),
    Some(3_600_000),
];

pub(crate) struct LayoutSection {
    scene: Arc<Mutex<SceneGraph>>,
    render_wake: RenderWakeNotifier,
    /// Saves a widget's entry; `None`: no config file, changes last the
    /// session.
    save: Option<WidgetWriter>,
    /// `[displays.<NAME>]` zone lists, as the HUD started.
    displays: Vec<(String, Vec<String>)>,
}

/// One widget as the rows need it.
struct Widget {
    name: String,
    tab: String,
    geometry: Option<GeometryPolicy>,
    auto_clear_ms: Option<u64>,
}

impl LayoutSection {
    const PREFIX: &'static str = "layout.";
    const GEOMETRY: &'static str = ".geometry";
    const CLEAR: &'static str = ".clear";

    pub(crate) fn new(
        scene: Arc<Mutex<SceneGraph>>,
        render_wake: RenderWakeNotifier,
        save: Option<WidgetWriter>,
        displays: Vec<(String, Vec<String>)>,
    ) -> Self {
        Self {
            scene,
            render_wake,
            save,
            displays,
        }
    }

    async fn widgets(&self) -> Vec<Widget> {
        let scene = self.scene.lock().await;
        let registry = &scene.widget_registry;
        let mut widgets: Vec<Widget> = registry
            .instances
            .values()
            .map(|i| Widget {
                name: i.instance_name.clone(),
                tab: scene
                    .tabs
                    .get(&i.tab_id)
                    .map(|t| t.name.clone())
                    .unwrap_or_default(),
                geometry: i.geometry_override.or_else(|| {
                    registry
                        .definitions
                        .get(&i.widget_type_name)
                        .map(|d| d.default_geometry_policy)
                }),
                auto_clear_ms: i.auto_clear_ms,
            })
            .collect();
        widgets.sort_by(|a, b| a.name.cmp(&b.name));
        widgets
    }

    fn saved_detail(&self) -> &'static str {
        if self.save.is_some() {
            "Saved to config"
        } else {
            "No config file; lasts until restart"
        }
    }

    /// Apply now, then save; a failed save keeps the change for the session.
    fn save(&self, widget: &Widget, update: &WidgetUpdate) -> Result<(), String> {
        let Some(save) = &self.save else {
            return Ok(());
        };
        save(&widget.tab, &widget.name, update)
            .map_err(|e| format!("Applied until restart; not saved: {e}"))
    }

    async fn find(&self, name: &str) -> Result<Widget, String> {
        self.widgets()
            .await
            .into_iter()
            .find(|w| w.name == name)
            .ok_or_else(|| format!("No widget {name}"))
    }
}

/// `0.0125` → `"1.25"`: a fraction as a percentage, without trailing zeros.
fn percent(v: f32) -> String {
    let s = format!("{:.2}", v * 100.0);
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The row's value: "4%, 8% · 18×12%", or where an edge-anchored widget sits.
fn describe(geometry: Option<GeometryPolicy>) -> String {
    match geometry {
        Some(GeometryPolicy::Relative {
            x_pct,
            y_pct,
            width_pct,
            height_pct,
        }) => format!(
            "{}%, {}% · {}×{}%",
            percent(x_pct),
            percent(y_pct),
            percent(width_pct),
            percent(height_pct)
        ),
        Some(GeometryPolicy::EdgeAnchored { edge, .. }) => {
            format!("Anchored {}", format!("{edge:?}").to_lowercase())
        }
        None => "Default".to_owned(),
    }
}

/// Four numbers typed as percentages of the display (x, y, width, height),
/// separated by anything that is not part of a number: "4 8 18 12",
/// "4%, 8% · 18×12%". A number typed as the row shows it keeps the exact
/// `current` value, so editing one component never rounds the others.
fn parse_box(text: &str, current: Option<GeometryPolicy>) -> Result<WidgetUpdate, String> {
    let numbers: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .filter(|s| !s.is_empty())
        .collect();
    let [x, y, w, h] = numbers[..] else {
        return Err("Type x, y, width and height in % of the display, e.g. 4 8 18 12".into());
    };
    let shown = match current {
        Some(GeometryPolicy::Relative {
            x_pct,
            y_pct,
            width_pct,
            height_pct,
        }) => Some([x_pct, y_pct, width_pct, height_pct]),
        _ => None,
    };
    let mut fractions = [0.0; 4];
    for (i, typed) in [x, y, w, h].into_iter().enumerate() {
        fractions[i] = match shown {
            Some(shown) if percent(shown[i]) == typed => shown[i],
            _ => {
                typed
                    .parse::<f32>()
                    .map_err(|_| format!("{typed} is not a number"))?
                    / 100.0
            }
        };
    }
    let [x_pct, y_pct, width_pct, height_pct] = fractions;
    let update = WidgetUpdate::Geometry {
        x_pct,
        y_pct,
        width_pct,
        height_pct,
    };
    update
        .validate()
        .map_err(|_| "The box must fit on the display, with a size above 0".to_owned())?;
    Ok(update)
}

fn clear_label(ms: Option<u64>) -> String {
    match ms {
        None => "never".to_owned(),
        Some(ms) if ms % 3_600_000 == 0 => format!("{} h", ms / 3_600_000),
        Some(ms) if ms % 60_000 == 0 => format!("{} min", ms / 60_000),
        Some(ms) if ms % 1_000 == 0 => format!("{} s", ms / 1_000),
        Some(ms) => format!("{ms} ms"),
    }
}

/// The choices, with `current` added when it is not one of them.
fn clear_options(current: Option<u64>) -> Vec<Option<u64>> {
    let mut options = CLEAR_CHOICES.to_vec();
    if !options.contains(&current) {
        options.push(current);
        options.sort_by_key(|ms| ms.unwrap_or(0));
    }
    options
}

impl Section for LayoutSection {
    fn id(&self) -> &'static str {
        "layout"
    }

    fn title(&self) -> &'static str {
        "Layout"
    }

    fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
        Box::pin(async move {
            let mut rows = Vec::new();
            for widget in self.widgets().await {
                let id = format!("{}{}", Self::PREFIX, widget.name);
                rows.push(
                    Row::new(
                        format!("{id}{}", Self::GEOMETRY),
                        format!("{} position and size", widget.name),
                        Control::Field {
                            text: describe(widget.geometry),
                        },
                    )
                    .detail(format!(
                        "x, y · width×height in % of the display · {}",
                        self.saved_detail()
                    ))
                    .keywords(&["widget", "move", "resize", "position", "size", "geometry"]),
                );
                let options = clear_options(widget.auto_clear_ms);
                rows.push(
                    Row::new(
                        format!("{id}{}", Self::CLEAR),
                        format!("{} clears after", widget.name),
                        Control::Choice {
                            selected: options
                                .iter()
                                .position(|o| *o == widget.auto_clear_ms)
                                .unwrap_or(0),
                            options: options.into_iter().map(clear_label).collect(),
                        },
                    )
                    .detail(format!(
                        "Each update stays this long unless the agent asks for less · {}",
                        self.saved_detail()
                    ))
                    .keywords(&[
                        "auto clear",
                        "timeout",
                        "ttl",
                        "expire",
                        "dismiss",
                    ]),
                );
            }
            for (display, zones) in &self.displays {
                rows.push(
                    Row::new(
                        format!("{}display.{display}", Self::PREFIX),
                        format!("Zones on {display}"),
                        Control::ReadOnly {
                            text: zones.join(", "),
                            mono: true,
                        },
                    )
                    .detail("Waits for stable monitor names; set in [displays] for now")
                    .keywords(&["monitor", "display", "screen", "zones"]),
                );
            }
            rows
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let described = format!("{edit:?}");
            let unsupported = || format!("{row} does not support {described}");
            let rest = row.strip_prefix(Self::PREFIX).ok_or_else(unsupported)?;
            if let Some(name) = rest.strip_suffix(Self::GEOMETRY) {
                let text = match &edit {
                    Edit::Text(text) => text,
                    // Enter without typing: nothing to change.
                    Edit::Activate => return Ok(()),
                    _ => return Err(unsupported()),
                };
                let widget = self.find(name).await?;
                let update = parse_box(text, widget.geometry)?;
                let WidgetUpdate::Geometry {
                    x_pct,
                    y_pct,
                    width_pct,
                    height_pct,
                } = update
                else {
                    unreachable!("parse_box returns a geometry");
                };
                self.scene.lock().await.set_widget_geometry(
                    name,
                    GeometryPolicy::Relative {
                        x_pct,
                        y_pct,
                        width_pct,
                        height_pct,
                    },
                );
                self.render_wake.notify();
                tracing::info!(widget = name, ?update, "control panel: widget moved");
                return self.save(&widget, &update);
            }
            if let Some(name) = rest.strip_suffix(Self::CLEAR) {
                let Edit::Choose(i) = edit else {
                    return Err(unsupported());
                };
                let widget = self.find(name).await?;
                let ms = *clear_options(widget.auto_clear_ms)
                    .get(i)
                    .ok_or("No such choice")?;
                self.scene.lock().await.set_widget_auto_clear(name, ms);
                self.render_wake.notify();
                tracing::info!(widget = name, ?ms, "control panel: widget auto-clear set");
                return self.save(&widget, &WidgetUpdate::AutoClearMs(ms));
            }
            Err(unsupported())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_box_is_typed_as_four_percentages() {
        let expected = WidgetUpdate::Geometry {
            x_pct: 0.04,
            y_pct: 0.08,
            width_pct: 0.18,
            height_pct: 0.12,
        };
        for typed in ["4 8 18 12", "4%, 8% · 18×12%", " 4,8,18,12 "] {
            assert_eq!(parse_box(typed, None), Ok(expected.clone()), "{typed}");
        }
        assert!(
            parse_box("4 8 18", None)
                .unwrap_err()
                .starts_with("Type x, y")
        );
        assert!(parse_box("90 8 18 12", None).unwrap_err().contains("fit"));
        assert!(parse_box("4 8 0 12", None).unwrap_err().contains("fit"));
        assert!(parse_box("-4 8 18 12", None).unwrap_err().contains("fit"));
        assert!(
            parse_box("4 8-1 18 12", None)
                .unwrap_err()
                .contains("not a number")
        );

        // Unchanged components keep their exact value; the typed one moves.
        let current = GeometryPolicy::Relative {
            x_pct: 1.0 / 3.0,
            y_pct: 0.08,
            width_pct: 0.18,
            height_pct: 0.123_456,
        };
        let typed = describe(Some(current)).replace("8%", "10%");
        assert_eq!(
            parse_box(&typed, Some(current)),
            Ok(WidgetUpdate::Geometry {
                x_pct: 1.0 / 3.0,
                y_pct: 0.10,
                width_pct: 0.18,
                height_pct: 0.123_456,
            })
        );
        assert_eq!(
            describe(Some(GeometryPolicy::Relative {
                x_pct: 0.0125,
                y_pct: 0.08,
                width_pct: 0.18,
                height_pct: 0.12,
            })),
            "1.25%, 8% · 18×12%"
        );
    }

    #[test]
    fn clear_choices_name_their_duration_and_keep_a_custom_one() {
        let labels: Vec<String> = clear_options(None).into_iter().map(clear_label).collect();
        assert_eq!(
            labels,
            [
                "never", "5 s", "10 s", "30 s", "1 min", "5 min", "15 min", "1 h"
            ]
        );
        let custom = clear_options(Some(8_000));
        assert_eq!(custom.len(), 9);
        assert_eq!(clear_label(custom[2]), "8 s");
    }
}
