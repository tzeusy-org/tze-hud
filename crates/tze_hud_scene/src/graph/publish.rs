//! Typed zone and widget publication: what agents send.
//!
//! [`SceneGraph::publish`] commits one [`Publication`] directly, without the
//! generic mutation batch. It keeps the checks the batch made for a publish:
//! a named lease must exist and be active and its lease must still be within
//! budget; the content checks are the zone and widget operations' own. A
//! committed publication takes a sequence number and a present
//! acknowledgement like an applied batch.

use std::collections::HashMap;

use super::SceneGraph;
use crate::mutation::MutationBatch;
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
    /// Commit `publication` for `publisher`. On error the scene is unchanged.
    pub fn publish(
        &mut self,
        publisher: &Publisher,
        publication: &Publication,
    ) -> Result<(), ValidationError> {
        self.publish_with_notification_id(publisher, publication, None)
    }

    pub(crate) fn publish_with_notification_id(
        &mut self,
        publisher: &Publisher,
        publication: &Publication,
        notification_id: Option<SceneId>,
    ) -> Result<(), ValidationError> {
        if let Some(lease_id) = publisher.lease_id {
            self.check_publishing_lease(lease_id, &publisher.namespace)?;
        }
        self.apply_publication(
            publication,
            &publisher.namespace,
            publisher.lease_id,
            publisher.origin,
            notification_id,
        )?;
        self.next_sequence_number();
        self.record_present_ack_batch(SceneId::new());
        Ok(())
    }

    /// The lease a publication names must exist, be active and be within
    /// its budget.
    fn check_publishing_lease(
        &self,
        lease_id: SceneId,
        namespace: &str,
    ) -> Result<(), ValidationError> {
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
        // A publication adds no tiles, nodes or textures; this only refuses a
        // lease already over its budget.
        let nothing = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: namespace.to_string(),
            mutations: Vec::new(),
            timing_hints: None,
            lease_id: Some(lease_id),
        };
        self.check_budget(&lease_id, &nothing)
            .map_err(|budget| ValidationError::BudgetExceeded {
                resource: format!("{budget}"),
            })
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
    use super::super::test_helpers::make_scene;
    use super::super::tests::make_subtitle_zone;
    use super::*;

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
    fn publish_commits_and_refuses_a_lease_that_is_gone_or_inactive() {
        let mut scene = make_scene();
        scene.register_zone(make_subtitle_zone());
        let lease = scene.grant_lease("agent", 60_000);
        let publisher = Publisher {
            lease_id: Some(lease),
            ..Publisher::namespace("agent")
        };
        let sequence = scene.sequence_number;
        scene.publish(&publisher, &subtitle("one")).unwrap();
        assert_eq!(scene.sequence_number, sequence + 1);
        assert_eq!(scene.drain_present_ack_batch_ids().len(), 1);
        assert_eq!(scene.zone_registry.active_for_zone("subtitle").len(), 1);

        scene.suspend_all_leases(scene.now_millis());
        let refused = scene.publish(&publisher, &subtitle("two"));
        assert!(matches!(refused, Err(ValidationError::InvalidField { .. })));
        scene.revoke_lease(lease).unwrap();
        let unknown = Publisher {
            lease_id: Some(SceneId::new()),
            ..Publisher::namespace("agent")
        };
        let refused = scene.publish(&unknown, &subtitle("three"));
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
}
