//! Durable fan-out for terminal lease transitions produced by the runtime.
//!
//! The compositor owns `SceneGraph::expire_leases()`, while each connected
//! session owns the ordered server stream to its agent. This small bridge keeps
//! the compositor out of per-session transport state and preserves terminal
//! events until the relevant session handler can emit its transactional
//! `LeaseResponse` messages.

use std::sync::{Arc, Mutex};

use tze_hud_scene::types::{LeaseExpiry, LeaseState, SceneId};

/// A terminal transition returned by `SceneGraph::expire_leases()`.
///
/// `removed_tiles` is retained for runtime diagnostics and tests. The owning
/// session is selected by `lease_id`, not by namespace, so a reconnecting agent
/// cannot receive another agent's terminal transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseExpiryNotice {
    pub lease_id: SceneId,
    pub previous_state: LeaseState,
    pub terminal_state: LeaseState,
    pub removed_tiles: Vec<SceneId>,
}

impl From<LeaseExpiry> for LeaseExpiryNotice {
    fn from(expiry: LeaseExpiry) -> Self {
        Self {
            lease_id: expiry.lease_id,
            previous_state: expiry.previous_state,
            terminal_state: expiry.terminal_state,
            removed_tiles: expiry.removed_tiles,
        }
    }
}

/// Runtime-side publisher for terminal lease events.
///
/// Each subscriber has a dedicated unbounded queue. Terminal lease transitions
/// are rare and cannot be dropped or coalesced; a slow session queues events
/// on its own lane rather than stalling the compositor while it holds a
/// scene-derived result. Dropping the receiver unregisters that lane promptly,
/// so failed handshakes and disconnected sessions do not remain in fan-out.
#[derive(Clone, Default)]
pub struct LeaseExpirySender {
    subscribers: Arc<Mutex<LeaseExpirySubscribers>>,
}

pub struct LeaseExpiryReceiver {
    rx: tokio::sync::mpsc::UnboundedReceiver<LeaseExpiryNotice>,
    subscriber_id: u64,
    subscribers: Arc<Mutex<LeaseExpirySubscribers>>,
}

#[derive(Default)]
struct LeaseExpirySubscribers {
    next_id: u64,
    senders: Vec<LeaseExpirySubscriber>,
}

struct LeaseExpirySubscriber {
    id: u64,
    tx: tokio::sync::mpsc::UnboundedSender<LeaseExpiryNotice>,
}

impl LeaseExpirySender {
    /// Subscribe before or during a connected session. The session handler
    /// filters notices by its owned lease ids before emitting them on the wire.
    /// Dropping the returned receiver unregisters it without requiring a later
    /// terminal transition.
    pub fn subscribe(&self) -> LeaseExpiryReceiver {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscribers = self
            .subscribers
            .lock()
            .expect("lease-expiry subscriber registry poisoned");
        let subscriber_id = subscribers.next_id;
        subscribers.next_id = subscribers
            .next_id
            .checked_add(1)
            .expect("lease-expiry subscriber id space exhausted");
        subscribers.senders.push(LeaseExpirySubscriber {
            id: subscriber_id,
            tx,
        });
        LeaseExpiryReceiver {
            rx,
            subscriber_id,
            subscribers: Arc::clone(&self.subscribers),
        }
    }

    /// Publish one terminal transition and return the number of live handlers
    /// that received it. Closed session queues are pruned atomically with this
    /// send pass.
    pub fn publish(&self, notice: LeaseExpiryNotice) -> usize {
        let mut subscribers = self
            .subscribers
            .lock()
            .expect("lease-expiry subscriber registry poisoned");
        let mut delivered = 0;
        subscribers.senders.retain(|subscriber| {
            if subscriber.tx.send(notice.clone()).is_ok() {
                delivered += 1;
                true
            } else {
                false
            }
        });
        delivered
    }
}

impl LeaseExpiryReceiver {
    pub async fn recv(&mut self) -> Option<LeaseExpiryNotice> {
        self.rx.recv().await
    }
}

impl Drop for LeaseExpiryReceiver {
    fn drop(&mut self) {
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers
                .senders
                .retain(|subscriber| subscriber.id != self.subscriber_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice() -> LeaseExpiryNotice {
        LeaseExpiryNotice {
            lease_id: SceneId::new(),
            previous_state: LeaseState::Active,
            terminal_state: LeaseState::Expired,
            removed_tiles: vec![SceneId::new()],
        }
    }

    #[tokio::test]
    async fn terminal_notice_is_durable_for_a_slow_connected_handler() {
        let sender = LeaseExpirySender::default();
        let mut receiver = sender.subscribe();
        let first = notice();
        let second = notice();

        assert_eq!(sender.publish(first.clone()), 1);
        assert_eq!(sender.publish(second.clone()), 1);
        assert_eq!(receiver.recv().await, Some(first));
        assert_eq!(receiver.recv().await, Some(second));
    }

    #[test]
    fn dropped_receiver_unregisters_without_waiting_for_an_expiry() {
        let sender = LeaseExpirySender::default();
        let baseline = sender
            .subscribers
            .lock()
            .expect("lease-expiry subscriber registry poisoned")
            .senders
            .len();

        let receiver = sender.subscribe();
        assert_eq!(
            sender
                .subscribers
                .lock()
                .expect("lease-expiry subscriber registry poisoned")
                .senders
                .len(),
            baseline + 1,
            "a live receiver must be registered"
        );

        drop(receiver);

        assert_eq!(
            sender
                .subscribers
                .lock()
                .expect("lease-expiry subscriber registry poisoned")
                .senders
                .len(),
            baseline,
            "a dropped receiver must unregister without waiting for an unrelated expiry"
        );
    }
}
