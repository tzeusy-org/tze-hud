//! Visibility: hide portals, zones or widgets from the screen, or all of
//! them at once (spec CP-6).
//!
//! Hiding is a view choice of the owner, not a scene change: hidden content is
//! not drawn, catches no input and holds no focus, but leases, publications
//! and agents are untouched and nothing tells an agent. The set lives in the
//! scene's runtime-only overlay state, so a restart shows everything again.

use std::sync::Arc;

use tokio::sync::Mutex;
use tze_hud_scene::graph::{SceneGraph, SurfaceClasses};
use tze_hud_scene::render_wake::RenderWakeNotifier;

use super::super::row::{BoxFuture, Control, Edit, Row, Section};

pub(crate) struct VisibilitySection {
    scene: Arc<Mutex<SceneGraph>>,
    render_wake: RenderWakeNotifier,
}

/// One row per surface class: (row id, label, what it covers, search terms).
type ClassRow = (
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
);

const CLASSES: [ClassRow; 3] = [
    (
        VisibilitySection::PORTALS,
        "Portals",
        "Session portals",
        &["session", "transcript", "chat"],
    ),
    (
        VisibilitySection::ZONES,
        "Zones",
        "Ambient zones: notifications, status, subtitles",
        &["notification", "status", "subtitle", "banner", "ambient"],
    ),
    (
        VisibilitySection::WIDGETS,
        "Widgets",
        "SVG widgets: gauges, progress, badges",
        &["gauge", "progress", "badge", "svg"],
    ),
];

fn any_class_hidden(hidden: SurfaceClasses) -> bool {
    CLASSES.iter().any(|(row, ..)| class_hidden(hidden, row))
}

fn class_hidden(set: SurfaceClasses, row: &str) -> bool {
    let mut set = set;
    class_mut(&mut set, row).is_some_and(|hidden| *hidden)
}

fn class_mut<'a>(set: &'a mut SurfaceClasses, row: &str) -> Option<&'a mut bool> {
    Some(match row {
        VisibilitySection::PORTALS => &mut set.portals,
        VisibilitySection::ZONES => &mut set.zones,
        VisibilitySection::WIDGETS => &mut set.widgets,
        _ => return None,
    })
}

/// What is hidden, as a sentence fragment: "Widgets", "Portals and zones",
/// "Portals, zones and widgets", or "Everything".
pub(crate) fn hidden_names(hidden: SurfaceClasses) -> String {
    if hidden.portals && hidden.zones && hidden.widgets {
        return "Everything".to_owned();
    }
    let names: Vec<String> = CLASSES
        .iter()
        .filter(|(row, ..)| class_hidden(hidden, row))
        .enumerate()
        .map(|(i, (_, label, ..))| {
            if i == 0 {
                (*label).to_owned()
            } else {
                label.to_lowercase()
            }
        })
        .collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

impl VisibilitySection {
    pub(crate) const HIDE_ALL: &'static str = "visibility.hide_all";
    pub(crate) const PORTALS: &'static str = "visibility.portals";
    pub(crate) const ZONES: &'static str = "visibility.zones";
    pub(crate) const WIDGETS: &'static str = "visibility.widgets";

    pub(crate) fn new(scene: Arc<Mutex<SceneGraph>>, render_wake: RenderWakeNotifier) -> Self {
        Self { scene, render_wake }
    }

    async fn hidden(&self) -> SurfaceClasses {
        self.scene.lock().await.owner_hidden()
    }
}

impl Section for VisibilitySection {
    fn id(&self) -> &'static str {
        "visibility"
    }

    fn title(&self) -> &'static str {
        "Visibility"
    }

    fn rows(&self) -> BoxFuture<'_, Vec<Row>> {
        Box::pin(async move {
            let hidden = self.hidden().await;
            let mut rows = vec![
                Row::new(
                    Self::HIDE_ALL,
                    "Hide all",
                    Control::Toggle(hidden.portals && hidden.zones && hidden.widgets),
                )
                .detail(if any_class_hidden(hidden) {
                    format!("{} hidden · agents keep running", hidden_names(hidden))
                } else {
                    "Hides every agent surface; agents keep running".to_owned()
                })
                .keywords(&["show", "presenting", "screen share", "clear", "declutter"])
                .caution(any_class_hidden(hidden))
                .resettable(any_class_hidden(hidden)),
            ];
            for (id, label, covers, keywords) in CLASSES {
                let off = class_hidden(hidden, id);
                rows.push(
                    Row::new(
                        id,
                        format!("Show {}", label.to_lowercase()),
                        Control::Toggle(!off),
                    )
                    .detail(if off {
                        "Hidden until restart · agents not told".to_owned()
                    } else {
                        covers.to_owned()
                    })
                    .keywords(keywords)
                    .caution(off)
                    .resettable(off),
                );
            }
            rows
        })
    }

    fn apply<'a>(&'a self, row: &'a str, edit: Edit) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut scene = self.scene.lock().await;
            let old = scene.owner_hidden();
            let mut new = old;
            match (row, edit.clone()) {
                // Hide everything, or show everything when it all is hidden.
                // Reset shows everything (Hide all) or the class.
                (Self::HIDE_ALL, Edit::Reset) => new = SurfaceClasses::NONE,
                (_, Edit::Reset) => match class_mut(&mut new, row) {
                    Some(hidden) => *hidden = false,
                    None => return Err(format!("{row} does not support {edit:?}")),
                },
                (Self::HIDE_ALL, Edit::Set(hide)) => {
                    new = if hide {
                        SurfaceClasses::ALL
                    } else {
                        SurfaceClasses::NONE
                    }
                }
                (_, Edit::Set(show)) => match class_mut(&mut new, row) {
                    Some(hidden) => *hidden = !show,
                    None => return Err(format!("{row} does not support {edit:?}")),
                },
                _ => return Err(format!("{row} does not support {edit:?}")),
            }
            if scene.set_owner_hidden(new) {
                drop(scene);
                tracing::info!(row, ?old, ?new, "control panel: visibility changed");
                self.render_wake.notify();
            }
            Ok(())
        })
    }

    fn closed_notice(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            let hidden = self.hidden().await;
            any_class_hidden(hidden).then(|| format!("{} hidden", hidden_names(hidden)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_names_read_as_a_phrase() {
        let only = |f: fn(&mut SurfaceClasses)| {
            let mut set = SurfaceClasses::NONE;
            f(&mut set);
            hidden_names(set)
        };
        assert_eq!(only(|s| s.widgets = true), "Widgets");
        assert_eq!(
            only(|s| {
                s.portals = true;
                s.zones = true;
            }),
            "Portals and zones"
        );
        assert_eq!(
            only(|s| {
                s.portals = true;
                s.zones = true;
                s.widgets = true;
            }),
            "Everything"
        );
        assert_eq!(hidden_names(SurfaceClasses::ALL), "Everything");
    }

    /// A hidden class offers Reset, which shows it; Hide all's Reset shows
    /// everything.
    #[tokio::test]
    async fn reset_shows_what_is_hidden() {
        let scene = Arc::new(Mutex::new(SceneGraph::new(800.0, 600.0)));
        let section = VisibilitySection::new(Arc::clone(&scene), RenderWakeNotifier::default());
        let resets = |rows: Vec<Row>| -> Vec<(String, bool)> {
            rows.into_iter().map(|r| (r.id, r.reset)).collect()
        };
        assert!(resets(section.rows().await).iter().all(|(_, reset)| !reset));
        section
            .apply(VisibilitySection::WIDGETS, Edit::Set(false))
            .await
            .unwrap();
        let rows = resets(section.rows().await);
        let offers = |id: &str| rows.iter().find(|(r, _)| r == id).unwrap().1;
        assert!(offers(VisibilitySection::WIDGETS) && offers(VisibilitySection::HIDE_ALL));
        assert!(!offers(VisibilitySection::ZONES));
        section
            .apply(VisibilitySection::WIDGETS, Edit::Reset)
            .await
            .unwrap();
        assert_eq!(section.hidden().await, SurfaceClasses::NONE);

        section
            .apply(VisibilitySection::HIDE_ALL, Edit::Set(true))
            .await
            .unwrap();
        section
            .apply(VisibilitySection::HIDE_ALL, Edit::Reset)
            .await
            .unwrap();
        assert_eq!(section.hidden().await, SurfaceClasses::NONE);

        // Showing all public classes leaves the internal tile mask untouched.
        section
            .apply(VisibilitySection::HIDE_ALL, Edit::Set(true))
            .await
            .unwrap();
        for (id, ..) in CLASSES {
            section.apply(id, Edit::Set(true)).await.unwrap();
        }
        let hidden = section.hidden().await;
        assert_eq!(
            hidden,
            SurfaceClasses {
                tiles: true,
                ..SurfaceClasses::NONE
            }
        );
        assert!(
            hidden.any(),
            "the scene still tracks its internal tile mask"
        );
        let rows = section.rows().await;
        assert!(rows.iter().all(|r| !r.caution && !r.reset));
        assert_eq!(rows[0].control, Control::Toggle(false));
        assert_eq!(
            rows[0].detail.as_deref(),
            Some("Hides every agent surface; agents keep running")
        );
        assert_eq!(
            section.closed_notice().await,
            None,
            "completed notice computation ignores only the internal tile bit"
        );
    }
}
