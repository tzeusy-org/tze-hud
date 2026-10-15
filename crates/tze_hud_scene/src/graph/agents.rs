//! One agent's whole presence: what it holds, and the owner's Clear.
//!
//! The control panel's Agents section lists each agent's holdings and lets the
//! owner clear everything one agent shows (docs/invariants.md §3). Portals are
//! not here: their surfaces belong to the portal driver's own namespace, and
//! the runtime reclaims them in the portal hub.

use std::collections::BTreeSet;

use super::SceneGraph;
use crate::mutation::SceneMutation;
use crate::types::{LeaseExpiry, LeaseState, SceneId};

/// What one agent holds in the scene right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentHoldings {
    /// Zones showing, or waiting to show (`delay_ms`), the agent's content.
    pub zones: usize,
    /// Widgets showing the agent's parameters.
    pub widgets: usize,
    /// Tiles under the agent's live leases.
    pub tiles: usize,
}

impl AgentHoldings {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// What an owner Clear removed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentReclaim {
    /// The agent's holdings just before the Clear.
    pub cleared: AgentHoldings,
    /// One per lease ended, for the runtime to forward to the owning session
    /// (which sends `Reclaimed{OVERRIDE}`).
    pub expiries: Vec<LeaseExpiry>,
}

impl SceneGraph {
    fn agent_lease_ids(&self, namespace: &str) -> Vec<SceneId> {
        self.leases
            .values()
            .filter(|l| l.namespace == namespace && !l.state.is_terminal())
            .map(|l| l.id)
            .collect()
    }

    /// Every namespace that holds something: a live lease, a zone or widget
    /// publication, or a scheduled batch.
    pub fn agent_namespaces(&self) -> BTreeSet<String> {
        let leases = self
            .leases
            .values()
            .filter(|l| !l.state.is_terminal())
            .map(|l| &l.namespace);
        let zones = self
            .zone_registry
            .active_publishes
            .values()
            .flatten()
            .map(|r| &r.publisher_namespace);
        let widgets = self
            .widget_registry
            .active_publishes
            .values()
            .flatten()
            .map(|r| &r.publisher_namespace);
        let scheduled = self
            .scheduled_batches
            .iter()
            .map(|s| &s.batch.agent_namespace);
        leases
            .chain(zones)
            .chain(widgets)
            .chain(scheduled)
            .cloned()
            .collect()
    }

    /// What `namespace` holds: zones (shown or scheduled), widgets and tiles.
    pub fn agent_holdings(&self, namespace: &str) -> AgentHoldings {
        let mut zones: BTreeSet<&str> = self
            .zone_registry
            .active_publishes
            .iter()
            .filter(|(_, records)| records.iter().any(|r| r.publisher_namespace == namespace))
            .map(|(zone, _)| zone.as_str())
            .collect();
        for scheduled in &self.scheduled_batches {
            if scheduled.batch.agent_namespace != namespace {
                continue;
            }
            for mutation in &scheduled.batch.mutations {
                if let SceneMutation::PublishToZone { zone_name, .. } = mutation {
                    zones.insert(zone_name);
                }
            }
        }
        let widgets = self
            .widget_registry
            .active_publishes
            .values()
            .filter(|records| records.iter().any(|r| r.publisher_namespace == namespace))
            .count();
        let leases = self.agent_lease_ids(namespace);
        let tiles = self
            .tiles
            .values()
            .filter(|t| leases.contains(&t.lease_id))
            .count();
        AgentHoldings {
            zones: zones.len(),
            widgets,
            tiles,
        }
    }

    /// The owner's Clear: remove everything `namespace` shows, now, with no
    /// agent veto. Every live lease (active, orphaned or suspended) ends
    /// `Revoked` with its tiles; every zone and widget publication the agent
    /// made, under any lease or none, goes; its scheduled batches are dropped.
    ///
    /// Clear is not a block: the agent's next publish or lease request is
    /// served as usual.
    pub fn reclaim_agent(&mut self, namespace: &str) -> AgentReclaim {
        let cleared = self.agent_holdings(namespace);
        let expiries = self
            .agent_lease_ids(namespace)
            .into_iter()
            .map(|id| self.reap_lease(id, LeaseState::Revoked))
            .collect();

        for records in self.zone_registry.active_publishes.values_mut() {
            records.retain(|r| r.publisher_namespace != namespace);
        }
        self.zone_registry
            .active_publishes
            .retain(|_, records| !records.is_empty());

        let mut touched_widgets = Vec::new();
        for (name, records) in &mut self.widget_registry.active_publishes {
            let before = records.len();
            records.retain(|r| r.publisher_namespace != namespace);
            if records.len() != before {
                touched_widgets.push(name.clone());
            }
        }
        self.widget_registry
            .active_publishes
            .retain(|_, records| !records.is_empty());
        for name in touched_widgets {
            self.refresh_widget_current_params(&name);
        }

        self.scheduled_batches
            .retain(|s| s.batch.agent_namespace != namespace);
        self.prune_publication_orphans();
        self.version += 1;
        AgentReclaim { cleared, expiries }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::super::test_helpers::scene_with_gauge;
    use super::*;
    use crate::mutation::MutationBatch;
    use crate::types::{
        ContentionPolicy, Rect, Rgba, WidgetParameterValue, ZoneContent, ZonePublishToken,
        ZoneRegistry,
    };

    fn note(text: &str) -> ZoneContent {
        ZoneContent::Notification(crate::types::NotificationPayload {
            text: text.into(),
            ..Default::default()
        })
    }

    fn level(v: f32) -> HashMap<String, WidgetParameterValue> {
        HashMap::from([("level".to_string(), WidgetParameterValue::F32(v))])
    }

    /// The owner's Clear removes every kind of content one agent holds (tiles
    /// under live leases in any state, active, suspended or orphaned; zone and
    /// widget publications with or without a lease, or orphaned with their
    /// session; scheduled publishes), reports a revoke per lease, and
    /// leaves another agent's content alone (hud-vniri.6, invariant §3).
    #[test]
    fn reclaim_agent_clears_everything_it_holds_and_nothing_else() {
        let (mut scene, tab) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 4 });
        scene.zone_registry = ZoneRegistry::with_defaults();
        let tile = |scene: &mut SceneGraph, ns: &str, lease| {
            scene
                .create_tile(tab, ns, lease, Rect::new(0.0, 0.0, 10.0, 10.0), 1)
                .unwrap()
        };
        let active = scene.grant_lease("agent", 60_000);
        let suspended = scene.grant_lease("agent", 60_000);
        let orphaned = scene.grant_lease("agent", 60_000);
        tile(&mut scene, "agent", active);
        tile(&mut scene, "agent", suspended);
        tile(&mut scene, "agent", orphaned);
        scene.suspend_lease(&suspended, 1).unwrap();
        scene.disconnect_lease(&orphaned, 1).unwrap();
        // A disconnected session's publication, waiting out its grace.
        let origin = SceneId::new();
        scene
            .publish_to_zone_inner(
                "subtitle",
                ZoneContent::StreamText("gone soon".into()),
                "agent",
                None,
                None,
                None,
                Vec::new(),
                None,
                Some(origin),
                true,
            )
            .unwrap();
        assert!(scene.orphan_publications(origin, 1, 60_000));
        // An MCP-style publication under no lease, and one under a lease.
        scene
            .publish_to_zone("notification-area", note("hi"), "agent", None, None, None)
            .unwrap();
        scene
            .publish_to_zone_for_lease(
                "pip",
                ZoneContent::SolidColor(Rgba::WHITE),
                "agent",
                None,
                None,
                None,
                Vec::new(),
                Some(active),
            )
            .unwrap();
        scene
            .publish_to_widget("gauge", level(0.5), "agent", None, 0, None)
            .unwrap();
        let later = scene.now_wall_us() + 60_000_000;
        scene.schedule_batch(
            later,
            MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: "agent".into(),
                mutations: vec![SceneMutation::PublishToZone {
                    zone_name: "status-bar".into(),
                    content: ZoneContent::StreamText("later".into()),
                    publish_token: ZonePublishToken { token: vec![1] },
                    merge_key: None,
                    expires_at_wall_us: None,
                    content_classification: None,
                    breakpoints: Vec::new(),
                    held: false,
                }],
                timing_hints: None,
                lease_id: None,
            },
        );

        let bystander = scene.grant_lease("other", 60_000);
        let bystander_tile = tile(&mut scene, "other", bystander);
        scene
            .publish_to_zone("notification-area", note("yo"), "other", None, None, None)
            .unwrap();
        scene
            .publish_to_widget("gauge", level(0.9), "other", None, 0, None)
            .unwrap();
        let other_before = scene.agent_holdings("other");
        assert_eq!(
            scene.agent_namespaces().into_iter().collect::<Vec<_>>(),
            ["agent", "other"]
        );

        let held = AgentHoldings {
            zones: 4,
            widgets: 1,
            tiles: 3,
        };
        assert_eq!(scene.agent_holdings("agent"), held);
        let version = scene.version;
        let reclaim = scene.reclaim_agent("agent");

        assert_eq!(reclaim.cleared, held);
        let mut ended: Vec<_> = reclaim.expiries.iter().map(|e| e.lease_id).collect();
        ended.sort();
        let mut expected = vec![active, suspended, orphaned];
        expected.sort();
        assert_eq!(ended, expected, "one revoke per live lease");
        assert!(
            reclaim
                .expiries
                .iter()
                .all(|e| e.terminal_state == LeaseState::Revoked && e.removed_tiles.len() == 1)
        );
        assert!(scene.agent_holdings("agent").is_empty());
        assert!(!scene.agent_namespaces().contains("agent"));
        assert!(
            scene.scheduled_batches.is_empty(),
            "the pending publish is gone"
        );
        assert!(
            !scene.publication_orphans.contains_key(&origin),
            "no grace timer outlives the publication"
        );
        assert!(scene.version > version);

        assert_eq!(scene.agent_holdings("other"), other_before);
        assert!(scene.tiles.contains_key(&bystander_tile));
        assert!(scene.lease_is_active(&bystander));
        assert_eq!(
            scene.widget_registry.instances["gauge"].current_params["level"],
            WidgetParameterValue::F32(0.9),
            "the widget shows the remaining publisher"
        );

        // Clear is not a block, and clearing nothing is harmless.
        let again = scene.grant_lease("agent", 60_000);
        assert!(scene.lease_is_active(&again));
        assert!(scene.reclaim_agent("nobody").expiries.is_empty());
    }
}
