//! Bounded, never-drop fan-out for transactional degradation notices.

use std::sync::{Arc, Mutex};

use crate::proto::session::{DegradationLevel, DegradationNotice};

const DEFAULT_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct DegradationNoticeSender {
    inner: Arc<Mutex<Inner>>,
    capacity: usize,
}

struct Inner {
    current: DegradationNotice,
    next_subscriber_id: u64,
    subscribers: Vec<Subscriber>,
}

struct Subscriber {
    id: u64,
    tx: tokio::sync::mpsc::Sender<DegradationNotice>,
}

pub struct DegradationNoticeReceiver {
    rx: tokio::sync::mpsc::Receiver<DegradationNotice>,
    subscriber_id: u64,
    inner: Arc<Mutex<Inner>>,
}

impl Default for DegradationNoticeSender {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl DegradationNoticeSender {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "degradation notice capacity must be non-zero");
        Self {
            inner: Arc::new(Mutex::new(Inner {
                current: DegradationNotice {
                    level: DegradationLevel::Normal as i32,
                    reason: "runtime operating normally".to_string(),
                    timestamp_wall_us: 0,
                },
                next_subscriber_id: 0,
                subscribers: Vec::new(),
            })),
            capacity,
        }
    }

    /// Atomically register for future transitions and capture current state.
    /// Dropping the returned receiver unregisters its session lane without
    /// requiring a later degradation transition.
    pub fn subscribe_with_current(&self) -> (DegradationNoticeReceiver, DegradationNotice) {
        let (tx, rx) = tokio::sync::mpsc::channel(self.capacity);
        let mut inner = self
            .inner
            .lock()
            .expect("degradation notice registry poisoned");
        let current = inner.current.clone();
        let subscriber_id = inner.next_subscriber_id;
        inner.next_subscriber_id = inner
            .next_subscriber_id
            .checked_add(1)
            .expect("degradation notice subscriber id space exhausted");
        inner.subscribers.push(Subscriber {
            id: subscriber_id,
            tx,
        });
        (
            DegradationNoticeReceiver {
                rx,
                subscriber_id,
                inner: Arc::clone(&self.inner),
            },
            current,
        )
    }

    pub fn current(&self) -> DegradationNotice {
        self.inner
            .lock()
            .expect("degradation notice registry poisoned")
            .current
            .clone()
    }

    /// Level-1 state-stream cadence gate. Transactional lanes never call this.
    pub fn should_emit_state_stream(&self, source_sequence: u64) -> bool {
        let level = self.current().level;
        level == DegradationLevel::Normal as i32 || source_sequence.is_multiple_of(2)
    }

    /// Publish from async session/runtime code with bounded backpressure.
    pub async fn publish(&self, notice: DegradationNotice) -> usize {
        let current = notice.clone();
        let subscribers = self.begin_publish(notice);
        let mut delivered = 0;
        for tx in subscribers {
            if tx.send(current.clone()).await.is_ok() {
                delivered += 1;
            }
        }
        self.prune_closed();
        delivered
    }

    /// Publish from the dedicated compositor thread. A full live queue blocks.
    pub fn publish_blocking(&self, notice: DegradationNotice) -> usize {
        let current = notice.clone();
        let subscribers = self.begin_publish(notice);
        let mut delivered = 0;
        for tx in subscribers {
            if tx.blocking_send(current.clone()).is_ok() {
                delivered += 1;
            }
        }
        self.prune_closed();
        delivered
    }

    fn begin_publish(
        &self,
        notice: DegradationNotice,
    ) -> Vec<tokio::sync::mpsc::Sender<DegradationNotice>> {
        let mut inner = self
            .inner
            .lock()
            .expect("degradation notice registry poisoned");
        inner.current = notice;
        inner
            .subscribers
            .iter()
            .map(|subscriber| subscriber.tx.clone())
            .collect()
    }

    fn prune_closed(&self) {
        self.inner
            .lock()
            .expect("degradation notice registry poisoned")
            .subscribers
            .retain(|subscriber| !subscriber.tx.is_closed());
    }
}

impl DegradationNoticeReceiver {
    pub async fn recv(&mut self) -> Option<DegradationNotice> {
        self.rx.recv().await
    }
}

impl Drop for DegradationNoticeReceiver {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner
                .subscribers
                .retain(|subscriber| subscriber.id != self.subscriber_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(level: DegradationLevel) -> DegradationNotice {
        DegradationNotice {
            level: level as i32,
            reason: format!("{level:?}"),
            timestamp_wall_us: 0,
        }
    }

    #[tokio::test]
    async fn subscribe_captures_current_before_future_transitions() {
        let sender = DegradationNoticeSender::new(2);
        sender
            .publish(notice(DegradationLevel::RenderingSimplified))
            .await;

        let (mut receiver, current) = sender.subscribe_with_current();
        assert_eq!(current.level, DegradationLevel::RenderingSimplified as i32);

        sender.publish(notice(DegradationLevel::Normal)).await;
        assert_eq!(
            receiver.recv().await.unwrap().level,
            DegradationLevel::Normal as i32
        );
    }

    #[tokio::test]
    async fn full_queue_backpressures_instead_of_dropping_notice() {
        let sender = DegradationNoticeSender::new(1);
        let (mut receiver, _) = sender.subscribe_with_current();
        sender
            .publish(notice(DegradationLevel::RenderingSimplified))
            .await;

        let publisher = {
            let sender = sender.clone();
            tokio::spawn(async move { sender.publish(notice(DegradationLevel::Normal)).await })
        };
        tokio::task::yield_now().await;
        assert!(
            !publisher.is_finished(),
            "a full live queue must backpressure"
        );

        assert_eq!(
            receiver.recv().await.unwrap().level,
            DegradationLevel::RenderingSimplified as i32
        );
        assert_eq!(publisher.await.unwrap(), 1);
        assert_eq!(
            receiver.recv().await.unwrap().level,
            DegradationLevel::Normal as i32
        );
    }

    #[tokio::test]
    async fn degradation_coalesces_only_the_state_stream_gate() {
        let sender = DegradationNoticeSender::new(1);
        assert!(sender.should_emit_state_stream(1));
        sender
            .publish(notice(DegradationLevel::RenderingSimplified))
            .await;
        assert!(!sender.should_emit_state_stream(1));
        assert!(sender.should_emit_state_stream(2));
    }

    #[tokio::test]
    async fn dropped_receiver_unregisters_only_its_own_lane_without_a_transition() {
        let sender = DegradationNoticeSender::new(1);
        let baseline = sender
            .inner
            .lock()
            .expect("degradation notice registry poisoned")
            .subscribers
            .len();

        let (receiver, _) = sender.subscribe_with_current();
        let (mut remaining_receiver, _) = sender.subscribe_with_current();
        assert_eq!(
            sender
                .inner
                .lock()
                .expect("degradation notice registry poisoned")
                .subscribers
                .len(),
            baseline + 2,
            "each live receiver must have its own registered lane"
        );

        drop(receiver);

        assert_eq!(
            sender
                .inner
                .lock()
                .expect("degradation notice registry poisoned")
                .subscribers
                .len(),
            baseline + 1,
            "dropping one receiver must unregister only its own lane without waiting for an unrelated transition"
        );

        assert_eq!(
            sender
                .publish(notice(DegradationLevel::RenderingSimplified))
                .await,
            1,
            "the remaining live receiver must still receive future transitions"
        );
        assert_eq!(
            remaining_receiver.recv().await.unwrap().level,
            DegradationLevel::RenderingSimplified as i32
        );
    }
}
