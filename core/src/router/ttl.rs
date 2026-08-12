//! Ask TTL sweeper (spec §3.2): expiry moves the ask to `failed`, emits `message.status`,
//! and decrements the asker's pending count. Guarantees no permanently-stuck pending state
//! (spec §12). Deadlines are in-memory: a run owns its messages, and a process restart is a
//! new run.

use std::sync::Arc;
use std::time::Duration;

use crate::router::{Router, lock};
use crate::types::id::MessageId;

pub(crate) const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Holds only a `Weak`: the sweeper dies with the router.
pub(crate) fn spawn_sweeper(router: &Arc<Router>) {
    let weak = Arc::downgrade(router);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let Some(router) = weak.upgrade() else {
                return;
            };
            router.sweep_expired().await;
        }
    });
}

impl Router {
    pub(crate) async fn sweep_expired(&self) {
        let now = tokio::time::Instant::now();
        let expired: Vec<MessageId> = {
            let deadlines = lock(&self.deadlines);
            deadlines
                .iter()
                .filter(|(_, deadline)| **deadline <= now)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in expired {
            tracing::info!(message = %id.0, "ask TTL expired; failing message");
            self.fail_message(&id).await;
        }
    }
}
