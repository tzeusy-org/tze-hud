//! Scene diff computation — detect what changed between two scene versions.

use crate::graph::SceneGraph;
use crate::types::*;
use serde::{Deserialize, Serialize};

/// A diff entry describing a single change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DiffEntry {
    TabAdded {
        id: SceneId,
    },
    TabRemoved {
        id: SceneId,
    },
    TileAdded {
        id: SceneId,
    },
    TileRemoved {
        id: SceneId,
    },
    TileBoundsChanged {
        id: SceneId,
        old: Rect,
        new: Rect,
    },
    TileZOrderChanged {
        id: SceneId,
        old: u32,
        new: u32,
    },
    NodeAdded {
        id: SceneId,
    },
    NodeRemoved {
        id: SceneId,
    },
    NodeDataChanged {
        id: SceneId,
    },
    ActiveTabChanged {
        old: Option<SceneId>,
        new: Option<SceneId>,
    },
    LeaseAdded {
        id: SceneId,
    },
    LeaseRemoved {
        id: SceneId,
    },
}

/// A complete diff between two scene graph states.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SceneDiff {
    pub from_version: u64,
    pub to_version: u64,
    pub entries: Vec<DiffEntry>,
}

impl SceneDiff {
    /// Compute the diff between two scene graph snapshots.
    pub fn compute(old: &SceneGraph, new: &SceneGraph) -> Self {
        let mut entries = Vec::new();

        // Active tab change
        if old.active_tab != new.active_tab {
            entries.push(DiffEntry::ActiveTabChanged {
                old: old.active_tab,
                new: new.active_tab,
            });
        }

        // Tab diffs
        for id in new.tabs.keys() {
            if !old.tabs.contains_key(id) {
                entries.push(DiffEntry::TabAdded { id: *id });
            }
        }
        for id in old.tabs.keys() {
            if !new.tabs.contains_key(id) {
                entries.push(DiffEntry::TabRemoved { id: *id });
            }
        }

        // Tile diffs
        for (id, new_tile) in &new.tiles {
            if let Some(old_tile) = old.tiles.get(id) {
                if old_tile.bounds != new_tile.bounds {
                    entries.push(DiffEntry::TileBoundsChanged {
                        id: *id,
                        old: old_tile.bounds,
                        new: new_tile.bounds,
                    });
                }
                if old_tile.z_order != new_tile.z_order {
                    entries.push(DiffEntry::TileZOrderChanged {
                        id: *id,
                        old: old_tile.z_order,
                        new: new_tile.z_order,
                    });
                }
            } else {
                entries.push(DiffEntry::TileAdded { id: *id });
            }
        }
        for id in old.tiles.keys() {
            if !new.tiles.contains_key(id) {
                entries.push(DiffEntry::TileRemoved { id: *id });
            }
        }

        // Node diffs
        for (id, new_node) in &new.nodes {
            if let Some(old_node) = old.nodes.get(id) {
                if old_node.data != new_node.data {
                    entries.push(DiffEntry::NodeDataChanged { id: *id });
                }
            } else {
                entries.push(DiffEntry::NodeAdded { id: *id });
            }
        }
        for id in old.nodes.keys() {
            if !new.nodes.contains_key(id) {
                entries.push(DiffEntry::NodeRemoved { id: *id });
            }
        }

        // Lease diffs
        for id in new.leases.keys() {
            if !old.leases.contains_key(id) {
                entries.push(DiffEntry::LeaseAdded { id: *id });
            }
        }
        for id in old.leases.keys() {
            if !new.leases.contains_key(id) {
                entries.push(DiffEntry::LeaseRemoved { id: *id });
            }
        }

        SceneDiff {
            from_version: old.version,
            to_version: new.version,
            entries,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_detects_tile_add() {
        let mut scene1 = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene1.create_tab("Main", 0).unwrap();
        let lease_id = scene1.grant_lease("test", 60_000);

        let scene_before = scene1.clone();

        scene1
            .create_tile(
                tab_id,
                "test",
                lease_id,
                Rect::new(0.0, 0.0, 100.0, 100.0),
                1,
            )
            .unwrap();

        let diff = SceneDiff::compute(&scene_before, &scene1);
        assert!(!diff.is_empty());
        assert!(
            diff.entries
                .iter()
                .any(|e| matches!(e, DiffEntry::TileAdded { .. }))
        );
    }

    #[test]
    fn test_diff_empty_when_no_changes() {
        let scene = SceneGraph::new(1920.0, 1080.0);
        let diff = SceneDiff::compute(&scene, &scene);
        assert!(diff.is_empty());
    }
}
