//! One agent's whole presence: what it holds, and the owner's Clear.
//!
//! The control panel's Agents section lists each agent's holdings and lets the
//! owner clear everything one agent shows (docs/invariants.md §3). Portals are
//! not here: their surfaces belong to the portal driver's own namespace, and
//! the runtime reclaims them in the portal hub.

use std::collections::BTreeSet;

use super::SceneGraph;
use super::publish::Publication;
use crate::scope::owner_agent;
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
    /// Internal terminal transitions for the leases ended by this Clear.
    pub expiries: Vec<LeaseExpiry>,
}

impl SceneGraph {
    fn lease_ids_where(&self, owns: impl Fn(&str) -> bool) -> Vec<SceneId> {
        self.leases
            .values()
            .filter(|l| owns(&l.namespace) && !l.state.is_terminal())
            .map(|l| l.id)
            .collect()
    }

    /// Every agent that holds something under any of its scopes: a live
    /// lease, a zone or widget publication, or a scheduled publication.
    pub fn agent_namespaces(&self) -> BTreeSet<String> {
        self.holding_namespaces()
            .into_iter()
            .map(|ns| owner_agent(&ns).to_owned())
            .collect()
    }

    /// Every exact namespace (scoped or not) that holds something.
    pub fn holding_namespaces(&self) -> BTreeSet<String> {
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
            .scheduled_publications
            .iter()
            .map(|s| &s.publisher.namespace);
        leases
            .chain(zones)
            .chain(widgets)
            .chain(scheduled)
            .cloned()
            .collect()
    }

    /// What `agent` holds across all its scopes: zones (shown or
    /// scheduled), widgets and tiles.
    pub fn agent_holdings(&self, agent: &str) -> AgentHoldings {
        self.holdings_where(|ns| owner_agent(ns) == agent)
    }

    /// What exactly `namespace` (one scope) holds.
    pub fn scope_holdings(&self, namespace: &str) -> AgentHoldings {
        self.holdings_where(|ns| ns == namespace)
    }

    fn holdings_where(&self, owns: impl Fn(&str) -> bool) -> AgentHoldings {
        let mut zones: BTreeSet<&str> = self
            .zone_registry
            .active_publishes
            .iter()
            .filter(|(_, records)| records.iter().any(|r| owns(&r.publisher_namespace)))
            .map(|(zone, _)| zone.as_str())
            .collect();
        let mut widgets: BTreeSet<&str> = self
            .widget_registry
            .active_publishes
            .iter()
            .filter(|(_, records)| records.iter().any(|r| owns(&r.publisher_namespace)))
            .map(|(widget, _)| widget.as_str())
            .collect();
        for scheduled in &self.scheduled_publications {
            if !owns(&scheduled.publisher.namespace) {
                continue;
            }
            match &scheduled.publication {
                Publication::Zone { zone_name, .. } => {
                    zones.insert(zone_name);
                }
                Publication::Widget { widget_name, .. } => {
                    widgets.insert(widget_name);
                }
            }
        }
        let leases = self.lease_ids_where(&owns);
        let tiles = self
            .tiles
            .values()
            .filter(|t| leases.contains(&t.lease_id))
            .count();
        AgentHoldings {
            zones: zones.len(),
            widgets: widgets.len(),
            tiles,
        }
    }

    /// The owner's Clear: remove everything `namespace` shows, now, with no
    /// agent veto. Every live lease (active, orphaned or suspended) ends
    /// `Revoked` with its tiles; every zone and widget publication the agent
    /// made, under any lease or none, goes; its scheduled publications are dropped.
    ///
    /// Clear is not a block: the agent's next publish or lease request is
    /// served as usual. Every scope of the agent is cleared.
    pub fn reclaim_agent(&mut self, agent: &str) -> AgentReclaim {
        self.reclaim_where(|ns| owner_agent(ns) == agent)
    }

    /// Release one MCP session scope: everything it shows or has scheduled
    /// goes as on [`Self::reclaim_agent`], its armed question waits end
    /// `gone: cleared`, and its unacknowledged input is discarded, since no
    /// consumer for it can exist any more.
    pub fn release_scope(&mut self, namespace: &str) -> AgentReclaim {
        for wait in self.notification_waits.values_mut() {
            if wait.namespace == namespace {
                wait.outcome = Some(super::NotificationOutcome::Gone(
                    super::NotificationGone::Cleared,
                ));
                wait.notified = false;
            }
        }
        let reclaim = self.reclaim_where(|ns| ns == namespace);
        self.pending_actions.remove(namespace);
        self.pending_action_notifiers.remove(namespace);
        self.session_labels.remove(namespace);
        reclaim
    }

    fn reclaim_where(&mut self, owns: impl Fn(&str) -> bool) -> AgentReclaim {
        let cleared = self.holdings_where(&owns);
        let expiries = self
            .lease_ids_where(&owns)
            .into_iter()
            .map(|id| self.reap_lease(id, LeaseState::Revoked))
            .collect();

        for records in self.zone_registry.active_publishes.values_mut() {
            records.retain(|r| !owns(&r.publisher_namespace));
        }
        self.zone_registry
            .active_publishes
            .retain(|_, records| !records.is_empty());

        let mut touched_widgets = Vec::new();
        for (name, records) in &mut self.widget_registry.active_publishes {
            let before = records.len();
            records.retain(|r| !owns(&r.publisher_namespace));
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

        let pending_waits: Vec<_> = self
            .scheduled_publications
            .iter()
            .filter_map(|s| s.notification_publication)
            .collect();
        self.scheduled_publications
            .retain(|s| !owns(&s.publisher.namespace));
        for id in pending_waits {
            if !self
                .scheduled_publications
                .iter()
                .any(|s| s.notification_publication == Some(id))
            {
                self.finish_notification_wait(
                    id,
                    super::NotificationOutcome::Gone(super::NotificationGone::Cleared),
                );
            }
        }
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
    use crate::types::{
        ContentionPolicy, Rect, Rgba, WidgetParameterValue, ZoneContent, ZoneRegistry,
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
    /// leaves another agent's content alone.
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
                None,
                None,
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
        scene.schedule_publication(
            later,
            crate::Publisher::namespace("agent"),
            Publication::Zone {
                zone_name: "status-bar".into(),
                content: ZoneContent::StreamText("later".into()),
                merge_key: None,
                expires_at_wall_us: None,
                content_classification: None,
                breakpoints: Vec::new(),
                held: false,
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
            scene.scheduled_publications.is_empty(),
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

    fn cards(scene: &SceneGraph) -> Vec<(String, Option<String>, String)> {
        scene
            .zone_registry
            .active_for_zone("notification-area")
            .iter()
            .map(|r| {
                let ZoneContent::Notification(n) = &r.content else {
                    unreachable!()
                };
                (
                    r.publisher_namespace.clone(),
                    r.merge_key.clone(),
                    n.text.clone(),
                )
            })
            .collect()
    }

    /// Two sessions of one agent each own a key slot, clear only their own
    /// cards, and still share the agent's stack publisher cap and Clear.
    #[test]
    fn session_scopes_own_their_cards_and_share_the_agent() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        scene.zone_registry = ZoneRegistry::with_defaults();
        let (a, b) = (
            crate::scope::scoped_namespace("claude", "a"),
            crate::scope::scoped_namespace("claude", "b"),
        );
        let build = || Some("build".to_string());
        for (ns, text) in [(&a, "a1"), (&b, "b1"), (&a, "a2")] {
            scene
                .publish_to_zone("notification-area", note(text), ns, build(), None, None)
                .unwrap();
        }
        assert_eq!(
            cards(&scene),
            [
                (a.clone(), build(), "a2".into()),
                (b.clone(), build(), "b1".into())
            ],
            "one key slot per scope"
        );
        scene
            .clear_zone_and_cancel_pending("notification-area", &a)
            .unwrap();
        assert_eq!(cards(&scene), [(b.clone(), build(), "b1".into())]);
        assert_eq!(
            scene.agent_namespaces().into_iter().collect::<Vec<_>>(),
            ["claude"]
        );
        assert_eq!(scene.agent_holdings("claude").zones, 1);
        assert!(scene.scope_holdings(&a).is_empty());

        // The stack's per-publisher cap counts the whole agent.
        let zone = scene
            .zone_registry
            .zones
            .get_mut("notification-area")
            .unwrap();
        zone.max_publishers = 3;
        for (ns, key) in [(&a, "k1"), (&b, "k2")] {
            scene
                .publish_to_zone(
                    "notification-area",
                    note(key),
                    ns,
                    Some(key.into()),
                    None,
                    None,
                )
                .unwrap();
        }
        assert!(
            scene
                .publish_to_zone(
                    "notification-area",
                    note("over"),
                    &a,
                    Some("x".into()),
                    None,
                    None
                )
                .is_err(),
            "sessions share their agent's cap"
        );
        scene
            .publish_to_zone("notification-area", note("own"), "other", None, None, None)
            .unwrap();

        // The owner's Clear ends every scope of the agent.
        assert_eq!(scene.reclaim_agent("claude").cleared.zones, 1);
        let left: Vec<_> = cards(&scene).into_iter().map(|c| c.0).collect();
        assert_eq!(left, ["other"]);
    }

    /// Releasing a scope removes its cards and delayed publishes, discards
    /// its queued input and ends its armed waits, leaving siblings alone.
    #[test]
    fn release_scope_ends_only_that_session() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        scene.zone_registry = ZoneRegistry::with_defaults();
        let (a, b) = (
            crate::scope::scoped_namespace("claude", "a"),
            crate::scope::scoped_namespace("claude", "b"),
        );
        for ns in [&a, &b, &"claude".to_string()] {
            scene
                .publish_to_zone("notification-area", note(ns), ns, None, None, None)
                .unwrap();
            scene.push_pending_action(super::super::PendingAction {
                publication_id: None,
                publisher_namespace: ns.clone(),
                zone_name: "notification-area".into(),
                kind: super::super::PendingActionKind::Dismiss,
                key: None,
            });
        }
        scene.schedule_publication(
            u64::MAX,
            crate::Publisher::namespace(a.clone()),
            Publication::Zone {
                zone_name: "subtitle".into(),
                content: ZoneContent::StreamText("later".into()),
                merge_key: None,
                expires_at_wall_us: None,
                content_classification: None,
                breakpoints: Vec::new(),
                held: false,
            },
        );
        let wait = SceneId::new();
        let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        scene.arm_notification_wait(wait, &a, active, u64::MAX, Default::default());

        let released = scene.release_scope(&a);
        assert_eq!(
            released.cleared.zones, 2,
            "the card and the delayed publish"
        );
        assert!(scene.scheduled_publications.is_empty());
        assert_eq!(
            scene.notification_wait_outcome(wait),
            Some(super::super::NotificationOutcome::Gone(
                super::super::NotificationGone::Cleared
            ))
        );
        assert_eq!(scene.take_notification_wait_notifiers().len(), 1);
        assert!(scene.take_pending_actions(&a).is_empty());
        assert_eq!(scene.take_pending_actions(&b).len(), 1);
        assert_eq!(scene.take_pending_actions("claude").len(), 1);
        let left: Vec<_> = cards(&scene).into_iter().map(|c| c.0).collect();
        assert_eq!(left, [b.clone(), "claude".to_string()]);
    }

    /// A labeled session's publications carry its label as their caption,
    /// fixed at publish; unscoped ones and a released session's carry none.
    #[test]
    fn session_labels_caption_new_publications() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        scene.zone_registry = ZoneRegistry::with_defaults();
        let a = crate::scope::scoped_namespace("claude", "a");
        let caption = |scene: &mut SceneGraph, ns: &str| {
            scene
                .publish_to_zone("notification-area", note(ns), ns, None, None, None)
                .unwrap();
            scene
                .zone_registry
                .active_for_zone("notification-area")
                .last()
                .unwrap()
                .caption
                .clone()
        };
        scene.set_session_label(&a, Some("tze-hud".into()));
        assert_eq!(caption(&mut scene, &a).as_deref(), Some("tze-hud"));
        assert_eq!(caption(&mut scene, "claude"), None);
        scene.set_session_label(&a, Some("renamed".into()));
        let shown = scene.zone_registry.active_for_zone("notification-area");
        assert_eq!(
            shown[0].caption.as_deref(),
            Some("tze-hud"),
            "fixed at publish"
        );
        scene.release_scope(&a);
        assert_eq!(
            caption(&mut scene, &a),
            None,
            "a released session's label is gone"
        );
    }
}
