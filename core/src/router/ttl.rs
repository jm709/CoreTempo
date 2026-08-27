//! Ask TTL sweeper (spec §3.2): expiry moves the ask to `failed`, emits `message.status`,
//! and decrements the asker's pending count. Guarantees no permanently-stuck pending state
//! (spec §12). Deadlines are in-memory: a run owns its messages, and a process restart is a
//! new run.
//!
//! Also the owed-ask watchdog (spec 2026-08-17 §4): fails owed asks on blocked
//! (past grace) or exited agents and pokes the queue worker when a re-nudge is
//! due. Expiry runs first in every tick, so a dead ask is never poked for.

use std::sync::Arc;
use std::time::Duration;

use crate::router::{FailReason, Router, lock};
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
            router.sweep_owed().await;
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
            let reason = FailReason::timeout(&id, self.workflow.ask_timeout);
            self.fail_message(&id, reason).await;
        }
    }
}
