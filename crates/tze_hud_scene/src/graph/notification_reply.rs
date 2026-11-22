use super::{SceneGraph, overlay::NotificationReplyView};
use crate::types::{SceneId, ZoneContent, ZonePublishRecord};

/// A focused, nonempty reply field defers removal without rewriting the
/// publisher's original wall deadline or the independent MCP wait deadline.
pub(super) fn zone_publication_expiry(
    record: &ZonePublishRecord,
    reply_holding_expiry: Option<SceneId>,
) -> Option<u64> {
    if reply_holding_expiry == Some(record.publication_id) {
        None
    } else {
        record.expires_at_wall_us
    }
}

impl SceneGraph {
    /// Wall deadline for stack animation and expiry processing.
    pub fn zone_publication_expiry(&self, record: &ZonePublishRecord) -> Option<u64> {
        zone_publication_expiry(record, self.overlay.notification_reply_holding_expiry())
    }

    /// Open, redraw, or collapse the owner-local reply field. Closing a field
    /// that held its card past the original deadline expires it in this turn.
    pub fn set_notification_reply_view(&mut self, view: Option<NotificationReplyView>) -> bool {
        if self.overlay.notification_reply == view {
            return true;
        }
        if let Some(view) = &view
            && (self.owner_hidden().zones
                || self.overlay.notification_choices.contains_key(&view.publication_id)
                || !self.zone_registry.active_publishes.values().flatten().any(|record| {
                    record.publication_id == view.publication_id
                        && self.zone_publication_expiry(record).is_none_or(|end| end > self.now_wall_us())
                        && matches!(&record.content, ZoneContent::Notification(card)
                            if card.actions.get(view.action_index).is_some_and(|action| action.reply.is_some()))
                }))
        {
            return false;
        }
        let previous_hold = self.overlay.notification_reply_holding_expiry();
        self.overlay.notification_reply = view;
        self.version += 1;
        if previous_hold.is_some()
            && previous_hold != self.overlay.notification_reply_holding_expiry()
        {
            self.drain_expired_zone_publications();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        TestClock,
        graph::{NotificationGone, NotificationOutcome, PendingActionKind},
        render_wake::RenderWakeNotifier,
        types::{
            ContentionPolicy, NotificationAction, NotificationPayload, Rect, ZoneHitRegion,
            ZoneInteractionKind, ZoneRegistry,
        },
    };
    use std::sync::{Arc, atomic::AtomicBool};

    const ZONE: &str = "notification-area";
    const PUBLISHER: &str = "reply-agent";
    const EXPIRY_US: u64 = 10_000;
    const TEXT: &str = "use the staging DB";

    fn publish(scene: &mut SceneGraph, id: SceneId, key: &str, expiry: u64) {
        scene
            .publish_to_zone_inner(
                ZONE,
                ZoneContent::Notification(NotificationPayload {
                    text: "Which database?".into(),
                    actions: vec![NotificationAction {
                        label: "Other…".into(),
                        reply: Some("database".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                PUBLISHER,
                Some(key.into()),
                Some(expiry),
                None,
                Vec::new(),
                None,
                None,
                false,
                Some(id),
                None,
                false,
            )
            .unwrap();
    }

    fn fixture(text: &str) -> (SceneGraph, TestClock, ZoneHitRegion) {
        let clock = TestClock::from_us(1_000);
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        scene.zone_registry = ZoneRegistry::with_defaults();
        scene
            .zone_registry
            .zones
            .get_mut(ZONE)
            .unwrap()
            .contention_policy = ContentionPolicy::Stack { max_depth: 1 };
        let id = SceneId::new();
        scene.arm_notification_wait(
            id,
            PUBLISHER,
            Arc::new(AtomicBool::new(true)),
            100_000,
            RenderWakeNotifier::new(|| {}),
        );
        publish(&mut scene, id, "build", EXPIRY_US);
        let region = ZoneHitRegion {
            publication_id: id,
            zone_name: ZONE.into(),
            published_at_wall_us: scene.now_wall_us(),
            publisher_namespace: PUBLISHER.into(),
            bounds: Rect::new(0.0, 0.0, 100.0, 30.0),
            kind: ZoneInteractionKind::Action { index: 0 },
            interaction_id: "reply action".into(),
            tab_order: 0,
        };
        scene.overlay.zone_hit_regions.push(region.clone());
        assert!(
            scene.set_notification_reply_view(Some(NotificationReplyView {
                publication_id: id,
                action_index: 0,
                text: text.into(),
                cursor_byte: text.len(),
                selection_anchor: text.len(),
                at_capacity: false,
            }))
        );
        (scene, clock, region)
    }

    #[test]
    fn only_open_nonempty_field_defers_expiry_and_blur_expires_at_once() {
        for (text, active_when_due) in [("", false), (TEXT, true)] {
            let (mut scene, clock, region) = fixture(text);
            assert_eq!(
                scene.next_publication_expiry_wall_us(),
                (!active_when_due).then_some(EXPIRY_US)
            );
            clock.set_us(EXPIRY_US);
            assert_eq!(
                scene.drain_expired_zone_publications(),
                usize::from(!active_when_due)
            );
            if active_when_due {
                let record = &scene.zone_registry.active_for_zone(ZONE)[0];
                assert_eq!(record.expires_at_wall_us, Some(EXPIRY_US));
                assert!(scene.is_unanswered_question(record));
                assert_eq!(scene.notification_wait_outcome(region.publication_id), None);
                assert!(scene.set_notification_reply_view(None));
            }
            assert!(scene.zone_registry.active_for_zone(ZONE).is_empty());
            assert!(scene.overlay.notification_reply.is_none());
            assert_eq!(
                scene.notification_wait_outcome(region.publication_id),
                Some(NotificationOutcome::Gone(NotificationGone::Expired))
            );
        }
    }

    #[test]
    fn held_field_sends_once_after_ttl_and_preserves_the_answered_exit() {
        let (mut scene, clock, region) = fixture(TEXT);
        clock.set_us(EXPIRY_US);
        assert!(
            scene
                .submit_notification_reply(&region, TEXT.into(), 40)
                .is_some()
        );
        assert!(scene.overlay.notification_reply.is_none());
        assert_eq!(
            scene.next_publication_expiry_wall_us(),
            Some(EXPIRY_US + 40_000)
        );
        assert_eq!(
            scene.notification_wait_outcome(region.publication_id),
            Some(NotificationOutcome::Answered)
        );
        let action = scene
            .notification_wait_answer(region.publication_id, PUBLISHER)
            .unwrap();
        assert_eq!(
            action.kind,
            PendingActionKind::Reply {
                callback_id: "database".into(),
                text: TEXT.into()
            }
        );
        assert!(
            scene
                .submit_notification_reply(&region, TEXT.into(), 40)
                .is_none()
        );
    }

    #[test]
    fn dismissal_of_an_open_field_returns_the_existing_dismiss_receipt() {
        let (mut scene, clock, mut region) = fixture(TEXT);
        clock.set_us(EXPIRY_US);
        region.kind = ZoneInteractionKind::Dismiss;
        scene.overlay.zone_hit_regions.push(region.clone());
        assert!(matches!(
            scene.activate_notification(&region, 0),
            Some(crate::graph::NotificationActivation::Callback(_))
        ));
        assert!(scene.overlay.notification_reply.is_none());
        assert!(scene.zone_registry.active_for_zone(ZONE).is_empty());
        assert_eq!(
            scene
                .notification_wait_answer(region.publication_id, PUBLISHER)
                .unwrap()
                .kind,
            PendingActionKind::Dismiss
        );
    }

    #[test]
    fn exact_card_removal_drops_held_field_and_completes_existing_wait() {
        for reason in [
            NotificationGone::Replaced,
            NotificationGone::Cleared,
            NotificationGone::Displaced,
        ] {
            let (mut scene, clock, region) = fixture(TEXT);
            clock.set_us(EXPIRY_US);
            match reason {
                NotificationGone::Replaced => {
                    publish(&mut scene, SceneId::new(), "build", EXPIRY_US * 2)
                }
                NotificationGone::Cleared => scene
                    .clear_notification_key(ZONE, PUBLISHER, "build")
                    .unwrap(),
                NotificationGone::Displaced => {
                    publish(&mut scene, SceneId::new(), "second", EXPIRY_US * 2)
                }
                NotificationGone::Expired => unreachable!(),
            }
            assert!(scene.overlay.notification_reply.is_none());
            assert_eq!(
                scene.notification_wait_outcome(region.publication_id),
                Some(NotificationOutcome::Gone(reason))
            );
            assert!(
                scene
                    .submit_notification_reply(&region, TEXT.into(), 0)
                    .is_none()
            );
            assert!(scene.take_pending_actions(PUBLISHER).is_empty());
        }
    }
}
