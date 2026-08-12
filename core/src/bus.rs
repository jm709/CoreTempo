//! In-process control-plane event bus. SOLE `seq` authority for a run.
//! `bus.reset` is synthesized per-consumer by SSE/Tauri bridges — never `publish`ed.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::time::Timestamp;
use crate::types::event::{Event, EventPayload};

#[derive(Clone)]
pub struct EventBus {
    inner: Arc<BusInner>,
}

struct BusInner {
    tx: tokio::sync::broadcast::Sender<Event>,
    seq: AtomicU64,
    ring: Mutex<VecDeque<Event>>,
}

impl Default for EventBus {
    fn default() -> EventBus {
        EventBus::new()
    }
}

impl EventBus {
    /// tokio broadcast channel capacity.
    pub const CAPACITY: usize = 1024;
    /// SSE replay ring length (events).
    pub const REPLAY_RING: usize = 1024;

    #[must_use]
    pub fn new() -> EventBus {
        let (tx, _) = tokio::sync::broadcast::channel(EventBus::CAPACITY);
        EventBus {
            inner: Arc::new(BusInner {
                tx,
                seq: AtomicU64::new(0),
                ring: Mutex::new(VecDeque::with_capacity(EventBus::REPLAY_RING)),
            }),
        }
    }

    /// Assigns the next seq (starting at 1), stamps `ts`, appends to the replay
    /// ring, and broadcasts. Returns the assigned seq.
    pub fn publish(&self, payload: EventPayload) -> u64 {
        let seq = self.inner.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let event = Event {
            seq,
            ts: Timestamp::now(),
            payload,
        };
        {
            let mut ring = self
                .inner
                .ring
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if ring.len() == EventBus::REPLAY_RING {
                ring.pop_front();
            }
            ring.push_back(event.clone());
        }
        let _ = self.inner.tx.send(event);
        seq
    }

    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.inner.tx.subscribe()
    }

    /// Events with `seq > since`. `None` => aged out of the ring — the caller
    /// must synthesize a `bus.reset` for its consumer.
    #[must_use]
    pub fn replay_since(&self, since: u64) -> Option<Vec<Event>> {
        let ring = self
            .inner
            .ring
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(first) = ring.front() {
            if first.seq > since + 1 {
                return None;
            }
        } else if self.last_seq() > since {
            return None;
        }
        Some(ring.iter().filter(|e| e.seq > since).cloned().collect())
    }

    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.inner.seq.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use crate::bus::EventBus;
    use crate::types::agent::AgentState;
    use crate::types::event::EventPayload;
    use crate::types::id::AgentId;

    fn state_payload(agent: &str, state: AgentState) -> EventPayload {
        EventPayload::AgentStateChanged {
            agent: AgentId(agent.into()),
            state,
        }
    }

    #[tokio::test]
    async fn publish_assigns_monotonic_seq_from_one() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        assert_eq!(bus.publish(state_payload("a", AgentState::Starting)), 1);
        assert_eq!(bus.publish(state_payload("a", AgentState::Idle)), 2);
        assert_eq!(bus.last_seq(), 2);
        assert_eq!(rx.recv().await.unwrap().seq, 1);
        assert_eq!(rx.recv().await.unwrap().seq, 2);
    }

    #[tokio::test]
    async fn replay_since_returns_gap_free_tail() {
        let bus = EventBus::new();
        for _ in 0..5 {
            bus.publish(state_payload("a", AgentState::Working));
        }
        let tail = bus.replay_since(2).unwrap();
        assert_eq!(
            tail.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert!(bus.replay_since(5).unwrap().is_empty());
    }

    #[tokio::test]
    async fn replay_since_none_when_aged_out() {
        let bus = EventBus::new();
        for _ in 0..(EventBus::REPLAY_RING + 10) {
            bus.publish(state_payload("a", AgentState::Working));
        }
        assert!(
            bus.replay_since(3).is_none(),
            "seq 4..=10 fell off the ring"
        );
        let last = bus.last_seq();
        assert!(bus.replay_since(last - 1).is_some());
    }
}
