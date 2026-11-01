//! Fan-out of runtime observations to every connected view.
//!
//! `broadcast` is the right primitive here: a slow or hidden view must never stall the runtime, and
//! a session switch must not be able to drop an event another view still needs.

use crate::types::RuntimeEvent;
use tokio::sync::broadcast;

const CAPACITY: usize = 4096;

#[derive(Clone, Debug)]
pub struct EventBus {
    tx: broadcast::Sender<RuntimeEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self { tx }
    }

    /// Returns `false` when there are no subscribers; the send is not an error condition.
    pub fn publish(&self, event: RuntimeEvent) -> bool {
        self.tx.send(event).is_ok()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.tx.subscribe()
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NoticeLevel;

    #[tokio::test]
    async fn events_reach_every_subscriber_independently() {
        let bus = EventBus::new();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.publish(RuntimeEvent::Notice {
            level: NoticeLevel::Info,
            text: "hello".into(),
        });
        assert!(matches!(
            a.recv().await.unwrap(),
            RuntimeEvent::Notice { .. }
        ));
        assert!(matches!(
            b.recv().await.unwrap(),
            RuntimeEvent::Notice { .. }
        ));
    }

    #[tokio::test]
    async fn publishing_without_subscribers_is_not_an_error() {
        let bus = EventBus::new();
        assert!(!bus.publish(RuntimeEvent::Shutdown));
    }
}
