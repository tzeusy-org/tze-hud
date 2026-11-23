//! Typed zone and widget publication: what agents send.
//!
//! [`SceneGraph::publish`] dispatches one [`Publication`] to the zone or widget
//! operation. A specified lease must be active. A successful commit gets a
//! sequence number and a presentation ID.

use std::collections::HashMap;

use super::SceneGraph;
use crate::types::{SceneId, WidgetParameterValue, ZoneContent};
use crate::validation::ValidationError;

/// A zone or widget publication.
#[derive(Clone, Debug, PartialEq)]
pub enum Publication {
    Zone {
        zone_name: String,
        content: ZoneContent,
        /// For MergeByKey contention: the key under which content is stored.
        merge_key: Option<String>,
        /// Wall-clock expiry (microseconds since epoch); cleared at or before it.
        expires_at_wall_us: Option<u64>,
        /// Optional opaque content classification tag.
        content_classification: Option<String>,
        /// Byte-offset breakpoints for word-by-word reveal of `StreamText`.
        breakpoints: Vec<u64>,
        /// No expiry at all: a notification skips its urgency-derived default
        /// and stays until cleared. Ignored when `expires_at_wall_us` is set.
        held: bool,
    },
    Widget {
        widget_name: String,
        params: HashMap<String, WidgetParameterValue>,
        merge_key: Option<String>,
        transition_ms: u32,
        /// Relative lifetime, counted from when the publication applies.
        ttl_us: Option<u64>,
        /// An explicit timestamp takes precedence over `ttl_us`.
        expires_at_wall_us: Option<u64>,
    },
}

impl Publication {
    /// The zone this publishes to, if it is a zone publication.
    pub fn zone_name(&self) -> Option<&str> {
        match self {
            Self::Zone { zone_name, .. } => Some(zone_name),
            Self::Widget { .. } => None,
        }
    }

    /// The widget instance this publishes to, if it is a widget publication.
    pub fn widget_name(&self) -> Option<&str> {
        match self {
            Self::Widget { widget_name, .. } => Some(widget_name),
            Self::Zone { .. } => None,
        }
    }
}

/// Who publishes: the namespace, and either the agent's lease (MCP) or the
/// resident session's publication origin (gRPC, unleased content only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publisher {
    pub namespace: String,
    pub lease_id: Option<SceneId>,
    pub origin: Option<SceneId>,
}

impl Publisher {
    /// A publisher with neither lease nor session origin.
    pub fn namespace(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            lease_id: None,
            origin: None,
        }
    }
}

impl SceneGraph {
    /// Commit `publication` for `publisher` and return its present acknowledgement ID.
    pub fn publish(
        &mut self,
        publisher: &Publisher,
        publication: &Publication,
    ) -> Result<SceneId, ValidationError> {
        let id = SceneId::new();
        self.publish_with_id(id, publisher, publication, None)?;
        Ok(id)
    }

    pub(crate) fn publish_with_id(
        &mut self,
        id: SceneId,
        publisher: &Publisher,
        publication: &Publication,
        notification_id: Option<SceneId>,
    ) -> Result<(), ValidationError> {
        if let Some(lease_id) = publisher.lease_id {
            self.check_publishing_lease(lease_id)?;
        }
        self.apply_publication(
            publication,
            &publisher.namespace,
            publisher.lease_id,
            publisher.origin,
            notification_id,
        )?;
        self.next_sequence_number();
        self.record_present_ack_batch(id);
        Ok(())
    }

    /// The lease a publication names must exist and be active.
    fn check_publishing_lease(&self, lease_id: SceneId) -> Result<(), ValidationError> {
        let Some(lease) = self.leases.get(&lease_id) else {
            return Err(ValidationError::LeaseNotFound { id: lease_id });
        };
        if !lease.is_mutations_allowed() {
            return Err(if lease.is_expired(self.now_millis()) {
                ValidationError::LeaseExpired { id: lease_id }
            } else {
                ValidationError::InvalidField {
                    field: "lease_state".into(),
                    reason: format!(
                        "lease {lease_id} is in {:?} state; mutations require Active state",
                        lease.state,
                    ),
                }
            });
        }
        Ok(())
    }

    /// The zone or widget operation itself.
    pub(crate) fn apply_publication(
        &mut self,
        publication: &Publication,
        namespace: &str,
        lease_id: Option<SceneId>,
        origin: Option<SceneId>,
        notification_id: Option<SceneId>,
    ) -> Result<(), ValidationError> {
        match publication {
            Publication::Zone {
                zone_name,
                content,
                merge_key,
                expires_at_wall_us,
                content_classification,
                breakpoints,
                held,
            } => {
                let breakpoints = if matches!(content, ZoneContent::StreamText(_)) {
                    breakpoints.clone()
                } else {
                    Vec::new()
                };
                self.publish_to_zone_inner(
                    zone_name,
                    content.clone(),
                    namespace,
                    merge_key.clone(),
                    *expires_at_wall_us,
                    content_classification.clone(),
                    breakpoints,
                    lease_id,
                    origin,
                    !*held,
                    notification_id,
                )
            }
            Publication::Widget {
                widget_name,
                params,
                merge_key,
                transition_ms,
                ttl_us,
                expires_at_wall_us,
            } => {
                let expiry = expires_at_wall_us
                    .or_else(|| ttl_us.map(|ttl| self.now_wall_us().saturating_add(ttl)));
                self.publish_to_widget_for_origin(
                    widget_name,
                    params.clone(),
                    namespace,
                    merge_key.clone(),
                    *transition_ms,
                    expiry,
                    lease_id,
                    origin,
                )
                .map(|_| ())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{make_scene, make_scene_with_clock, scene_with_gauge};
    use super::super::tests::make_subtitle_zone;
    use super::*;
    use crate::types::ContentionPolicy;

    fn subtitle(text: &str) -> Publication {
        Publication::Zone {
            zone_name: "subtitle".into(),
            content: ZoneContent::StreamText(text.into()),
            merge_key: None,
            expires_at_wall_us: None,
            content_classification: None,
            breakpoints: Vec::new(),
            held: false,
        }
    }

    #[test]
    fn publish_commits_only_with_an_active_lease_when_named() {
        let (mut scene, clock) = make_scene_with_clock();
        scene.register_zone(make_subtitle_zone());
        let lease = scene.grant_lease("agent", 60_000);
        let publisher = Publisher {
            lease_id: Some(lease),
            ..Publisher::namespace("agent")
        };
        let sequence = scene.sequence_number;
        let id = scene.publish(&publisher, &subtitle("one")).unwrap();
        assert_eq!(scene.sequence_number, sequence + 1);
        assert_eq!(scene.drain_present_ack_batch_ids(), vec![id]);
        assert_eq!(scene.zone_registry.active_for_zone("subtitle").len(), 1);

        scene.suspend_all_leases(scene.now_millis());
        let due_id =
            scene.schedule_publication(scene.now_wall_us(), publisher.clone(), subtitle("two"));
        let due = scene.apply_due_publications();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, due_id);
        assert!(matches!(
            due[0].result,
            Err(ValidationError::InvalidField { .. })
        ));
        scene.resume_all_leases(scene.now_millis());
        clock.advance(60_001);
        assert_eq!(scene.expire_leases()[0].lease_id, lease);
        assert!(matches!(
            scene.publish(&publisher, &subtitle("three")),
            Err(ValidationError::LeaseExpired { id }) if id == lease
        ));
        let unknown = Publisher {
            lease_id: Some(SceneId::new()),
            ..Publisher::namespace("agent")
        };
        let refused = scene.publish(&unknown, &subtitle("four"));
        assert!(matches!(
            refused,
            Err(ValidationError::LeaseNotFound { .. })
        ));
        assert_eq!(
            scene.sequence_number,
            sequence + 1,
            "refusals commit nothing"
        );
        assert!(scene.drain_present_ack_batch_ids().is_empty());
    }

    #[test]
    fn refused_first_stack_zone_publication_creates_no_entry() {
        let mut scene = make_scene();
        let mut zone = make_subtitle_zone();
        zone.contention_policy = ContentionPolicy::Stack { max_depth: 4 };
        zone.max_publishers = 0;
        scene.register_zone(zone);
        let version = scene.version;

        assert_eq!(
            scene.publish(&Publisher::namespace("agent"), &subtitle("refused")),
            Err(ValidationError::ZoneMaxPublishersReached {
                zone: "subtitle".into(),
                max: 0,
            }),
        );
        assert!(
            !scene
                .zone_registry
                .active_publishes
                .contains_key("subtitle")
        );
        assert_eq!(scene.version, version);
        assert!(scene.drain_present_ack_batch_ids().is_empty());
    }

    #[test]
    fn refused_first_stack_widget_publication_creates_no_entry() {
        let (mut scene, _) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 4 });
        scene
            .widget_registry
            .definitions
            .get_mut("gauge")
            .unwrap()
            .max_publishers = 0;
        let version = scene.version;
        let publication = Publication::Widget {
            widget_name: "gauge".into(),
            params: HashMap::from([("level".into(), WidgetParameterValue::F32(0.5))]),
            merge_key: None,
            transition_ms: 0,
            ttl_us: None,
            expires_at_wall_us: None,
        };

        assert_eq!(
            scene.publish(&Publisher::namespace("agent"), &publication),
            Err(ValidationError::WidgetMaxPublishersReached {
                widget: "gauge".into(),
                max: 0,
            }),
        );
        assert!(!scene.widget_registry.active_publishes.contains_key("gauge"));
        assert_eq!(scene.version, version);
        assert_eq!(
            scene.widget_registry.instances["gauge"].current_params["level"],
            WidgetParameterValue::F32(0.0)
        );
        assert!(scene.drain_present_ack_batch_ids().is_empty());
    }
}
