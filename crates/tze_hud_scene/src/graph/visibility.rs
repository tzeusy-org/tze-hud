//! Owner visibility (T11, hud-vniri.4): the human can hide whole surface
//! classes for the session. Hidden content is not drawn, catches no pointer
//! input and holds no keyboard focus; leases, publications and agents are
//! untouched, and agents are not told. The set lives in the runtime overlay
//! (never serialized, never in a snapshot) and resets on restart.

use super::*;

/// The kinds of agent content the owner can hide.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SurfaceClasses {
    /// Session portals: every tile of a lease that carries a portal surface
    /// or a local scroll config.
    pub portals: bool,
    /// Zone publications.
    pub zones: bool,
    /// Widget instances.
    pub widgets: bool,
    /// Agent tiles that are not part of a portal.
    pub tiles: bool,
}

impl SurfaceClasses {
    pub const NONE: Self = Self {
        portals: false,
        zones: false,
        widgets: false,
        tiles: false,
    };
    pub const ALL: Self = Self {
        portals: true,
        zones: true,
        widgets: true,
        tiles: true,
    };

    pub fn any(self) -> bool {
        self != Self::NONE
    }

    pub fn all(self) -> bool {
        self == Self::ALL
    }
}

impl SceneGraph {
    /// The surface classes the owner has hidden.
    pub fn owner_hidden(&self) -> SurfaceClasses {
        self.overlay.owner_hidden
    }

    /// Set the hidden classes; returns true when that changed. A change bumps
    /// `version` so the compositor redraws (and re-primes its caches) once;
    /// nothing agent-visible changes.
    pub fn set_owner_hidden(&mut self, hidden: SurfaceClasses) -> bool {
        if self.overlay.owner_hidden == hidden {
            return false;
        }
        self.overlay.owner_hidden = hidden;
        self.version += 1;
        true
    }

    /// Whether `tile` belongs to a session portal: some tile of its lease
    /// carries a portal surface or a local scroll config.
    pub fn tile_is_portal(&self, tile: &Tile) -> bool {
        let marks = |id: SceneId| {
            self.overlay.tile_scroll_configs.contains_key(&id) || self.portal_surface(id).is_some()
        };
        marks(tile.id)
            || self
                .tiles
                .values()
                .any(|t| t.lease_id == tile.lease_id && t.id != tile.id && marks(t.id))
    }

    /// Whether `tile` is drawn and interactive under the owner's visibility.
    pub fn tile_shown(&self, tile: &Tile) -> bool {
        let hidden = self.overlay.owner_hidden;
        if !hidden.portals && !hidden.tiles {
            return true;
        }
        if self.tile_is_portal(tile) {
            !hidden.portals
        } else {
            !hidden.tiles
        }
    }

    /// [`Self::tile_shown`] by id (`false` for an unknown tile).
    pub fn tile_id_shown(&self, tile_id: SceneId) -> bool {
        self.tiles.get(&tile_id).is_some_and(|t| self.tile_shown(t))
    }

    /// Zone publications are drawn and interactive.
    pub fn zones_shown(&self) -> bool {
        !self.overlay.owner_hidden.zones
    }

    /// Zone publications to draw and hit-test: all of them, or none while the
    /// owner hides zones.
    pub fn shown_zone_publishes(
        &self,
    ) -> impl Iterator<Item = (&String, &Vec<ZonePublishRecord>)> + '_ {
        let shown = self.zones_shown();
        self.zone_registry
            .active_publishes
            .iter()
            .filter(move |_| shown)
    }

    /// Widget instances are drawn, rasterized and interactive.
    pub fn widgets_shown(&self) -> bool {
        !self.overlay.owner_hidden.widgets
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{HitResult, Rect, TileScrollConfig};

    /// A scene with one portal tile (scroll config) and one plain tile, side
    /// by side, both opaque to input.
    fn scene() -> (SceneGraph, SceneId, SceneId) {
        let mut scene = SceneGraph::new(1000.0, 500.0);
        let tab = scene.create_tab("Main", 0).unwrap();
        let portal_lease = scene.grant_lease("portal-agent", 60_000);
        let portal = scene
            .create_tile(
                tab,
                "portal-agent",
                portal_lease,
                Rect::new(0.0, 0.0, 400.0, 400.0),
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(
                portal,
                TileScrollConfig {
                    scrollable_x: false,
                    scrollable_y: true,
                    content_width: None,
                    content_height: None,
                },
            )
            .unwrap();
        let tile_lease = scene.grant_lease("tile-agent", 60_000);
        let tile = scene
            .create_tile(
                tab,
                "tile-agent",
                tile_lease,
                Rect::new(500.0, 0.0, 400.0, 400.0),
                1,
            )
            .unwrap();
        (scene, portal, tile)
    }

    #[test]
    fn classes_hide_their_own_tiles_only() {
        let (mut scene, portal, tile) = scene();
        assert!(scene.tile_id_shown(portal) && scene.tile_id_shown(tile));

        scene.set_owner_hidden(SurfaceClasses {
            portals: true,
            ..SurfaceClasses::NONE
        });
        assert!(!scene.tile_id_shown(portal), "portal hidden with portals");
        assert!(scene.tile_id_shown(tile), "plain tile is not a portal");
        assert!(scene.zones_shown() && scene.widgets_shown());

        scene.set_owner_hidden(SurfaceClasses {
            tiles: true,
            ..SurfaceClasses::NONE
        });
        assert!(scene.tile_id_shown(portal), "a portal is not a plain tile");
        assert!(!scene.tile_id_shown(tile));

        scene.set_owner_hidden(SurfaceClasses::ALL);
        assert!(!scene.tile_id_shown(portal) && !scene.tile_id_shown(tile));
        assert!(!scene.zones_shown() && !scene.widgets_shown());
        assert_eq!(scene.shown_zone_publishes().count(), 0);
    }

    /// A portal tile without its own marker (a second tile of the same lease)
    /// still hides with portals.
    #[test]
    fn every_tile_of_a_portal_lease_is_a_portal() {
        let (mut scene, portal, _) = scene();
        let (tab, lease) = {
            let t = &scene.tiles[&portal];
            (t.tab_id, t.lease_id)
        };
        let sibling = scene
            .create_tile(
                tab,
                "portal-agent",
                lease,
                Rect::new(0.0, 420.0, 100.0, 50.0),
                2,
            )
            .unwrap();
        scene.set_owner_hidden(SurfaceClasses {
            portals: true,
            ..SurfaceClasses::NONE
        });
        assert!(!scene.tile_id_shown(sibling));
    }

    #[test]
    fn hidden_tiles_pass_pointer_input_through() {
        let (mut scene, portal, tile) = scene();
        assert!(
            matches!(scene.hit_test(10.0, 10.0), HitResult::TileHit { tile_id, .. } if tile_id == portal)
        );

        scene.set_owner_hidden(SurfaceClasses {
            portals: true,
            ..SurfaceClasses::NONE
        });
        assert_eq!(scene.hit_test(10.0, 10.0), HitResult::Passthrough);
        assert!(
            matches!(scene.hit_test(510.0, 10.0), HitResult::TileHit { tile_id, .. } if tile_id == tile)
        );
    }

    /// Hiding leaves the scene's content alone but bumps the render version
    /// once per real change; restoring shows the same tiles again.
    #[test]
    fn a_change_bumps_the_version_once_and_keeps_content() {
        let (mut scene, ..) = scene();
        let (version, tiles, leases) = (scene.version, scene.tiles.clone(), scene.leases.clone());

        assert!(scene.set_owner_hidden(SurfaceClasses::ALL));
        assert_eq!(scene.version, version + 1);
        assert!(!scene.set_owner_hidden(SurfaceClasses::ALL), "no-op");
        assert_eq!(scene.version, version + 1, "a no-op does not repaint");
        assert_eq!(scene.tiles, tiles);
        assert_eq!(scene.leases, leases);

        assert!(scene.set_owner_hidden(SurfaceClasses::NONE));
        assert!(scene.tiles.keys().all(|id| scene.tile_id_shown(*id)));
    }

    #[test]
    fn owner_hiding_is_never_serialized() {
        let (mut scene, ..) = scene();
        scene.set_owner_hidden(SurfaceClasses::ALL);
        let json = serde_json::to_string(&scene).unwrap();
        let restored: SceneGraph = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.owner_hidden(), SurfaceClasses::NONE);
    }
}
