//! Traffic-class-aware fan-out for runtime-produced input events.
//!
//! Transactional input must survive a slow session consumer. Ephemeral and
//! state-stream input may use the bounded broadcast lane and report lag.

use std::sync::{Arc, Mutex};

use crate::proto::{EventBatch, input_envelope};

type AddressedBatch = (String, EventBatch);

#[derive(Clone)]
pub struct InputEventSender {
    inner: Arc<InputEventBusInner>,
}

struct InputEventBusInner {
    ephemeral_tx: tokio::sync::broadcast::Sender<AddressedBatch>,
    transactional_subscribers: Mutex<Vec<TransactionalSubscriber>>,
}

struct TransactionalSubscriber {
    namespace: Option<String>,
    tx: tokio::sync::mpsc::UnboundedSender<AddressedBatch>,
}

pub struct InputEventReceiver {
    ephemeral_rx: tokio::sync::broadcast::Receiver<AddressedBatch>,
    transactional_rx: tokio::sync::mpsc::UnboundedReceiver<AddressedBatch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEventRecvError {
    Lagged(u64),
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEventTryRecvError {
    Empty,
    Lagged(u64),
    Closed,
}

impl InputEventSender {
    pub fn new(ephemeral_capacity: usize) -> Self {
        let (ephemeral_tx, _) = tokio::sync::broadcast::channel(ephemeral_capacity);
        Self {
            inner: Arc::new(InputEventBusInner {
                ephemeral_tx,
                transactional_subscribers: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn subscribe(&self, namespace: impl Into<String>) -> InputEventReceiver {
        self.subscribe_inner(Some(namespace.into()))
    }

    /// Subscribe without namespace filtering for focused transport tests.
    #[doc(hidden)]
    pub fn subscribe_all(&self) -> InputEventReceiver {
        self.subscribe_inner(None)
    }

    fn subscribe_inner(&self, namespace: Option<String>) -> InputEventReceiver {
        let (transactional_tx, transactional_rx) = tokio::sync::mpsc::unbounded_channel();
        self.inner
            .transactional_subscribers
            .lock()
            .expect("input-event subscriber registry poisoned")
            .push(TransactionalSubscriber {
                namespace,
                tx: transactional_tx,
            });
        InputEventReceiver {
            ephemeral_rx: self.inner.ephemeral_tx.subscribe(),
            transactional_rx,
        }
    }

    /// Send an addressed batch and return the number of live subscribers.
    ///
    /// Transactional batches use per-subscriber unbounded queues: a slow
    /// consumer applies memory backpressure but cannot make the producer drop
    /// an event. Other batches retain bounded broadcast/latest-wins behavior.
    pub fn send(&self, item: AddressedBatch) -> usize {
        if batch_is_transactional(&item.1) {
            let mut subscribers = self
                .inner
                .transactional_subscribers
                .lock()
                .expect("input-event subscriber registry poisoned");
            let mut delivered = 0;
            subscribers.retain(|subscriber| {
                if subscriber
                    .namespace
                    .as_deref()
                    .is_some_and(|namespace| namespace != item.0)
                {
                    return true;
                }
                if subscriber.tx.send(item.clone()).is_ok() {
                    delivered += 1;
                    true
                } else {
                    false
                }
            });
            delivered
        } else {
            self.inner.ephemeral_tx.send(item).unwrap_or_default()
        }
    }
}

impl InputEventReceiver {
    pub async fn recv(&mut self) -> Result<AddressedBatch, InputEventRecvError> {
        tokio::select! {
            biased;
            transactional = self.transactional_rx.recv() => {
                transactional.ok_or(InputEventRecvError::Closed)
            }
            ephemeral = self.ephemeral_rx.recv() => {
                match ephemeral {
                    Ok(item) => Ok(item),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        Err(InputEventRecvError::Lagged(count))
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        Err(InputEventRecvError::Closed)
                    }
                }
            }
        }
    }

    pub fn try_recv(&mut self) -> Result<AddressedBatch, InputEventTryRecvError> {
        let transactional_closed = match self.transactional_rx.try_recv() {
            Ok(item) => return Ok(item),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => true,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => false,
        };
        match self.ephemeral_rx.try_recv() {
            Ok(item) => Ok(item),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) if transactional_closed => {
                Err(InputEventTryRecvError::Closed)
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                Err(InputEventTryRecvError::Empty)
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(count)) => {
                Err(InputEventTryRecvError::Lagged(count))
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                Err(InputEventTryRecvError::Closed)
            }
        }
    }
}

fn batch_is_transactional(batch: &EventBatch) -> bool {
    batch
        .events
        .iter()
        .filter_map(|envelope| envelope.event.as_ref())
        .any(|event| {
            !matches!(
                event,
                input_envelope::Event::PointerMove(_)
                    | input_envelope::Event::PointerEnter(_)
                    | input_envelope::Event::PointerLeave(_)
                    | input_envelope::Event::ScrollOffsetChanged(_)
                    | input_envelope::Event::ComposerDraftState(_)
            )
        })
}
