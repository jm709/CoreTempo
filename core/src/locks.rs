//! Per-agent readers/writers locks (multi-flow spec §4–5).
//!
//! One `tokio::sync::RwLock<()>` per pool agent. A flow acquires its members'
//! locks in sorted agent-id order — `read()` for `shared`, `write()` for
//! `exclusive`. Sorted acquisition prevents deadlock; tokio's `RwLock` is
//! FIFO-fair and write-preferring, so queue order holds and writers never
//! starve behind readers. Guards are owned so callers can move them into a
//! spawned run task (serve mode spawns runs un-awaited).
//!
//! Two independent instances exist at once by design: the serve daemon's
//! scheduler table and each warm run's `ApiContext` table each guard their
//! own roster (spec §5).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use crate::types::config::{AgentConcurrency, AgentConfig};
use crate::types::id::AgentId;

/// One lock per pool agent, with the agent's declared concurrency mode.
pub struct AgentLocks {
    locks: BTreeMap<AgentId, (AgentConcurrency, Arc<RwLock<()>>)>,
}

/// Guards for one acquisition; every held lock releases on drop.
#[must_use = "the guards must stay alive for as long as the flow holds its members; \
              dropping them releases every lock immediately"]
pub struct MemberGuards {
    _guards: Vec<MemberGuard>,
}

/// Each variant exists only to hold a guard alive until it drops; nothing
/// ever reads the payload back out, hence the expectation.
#[expect(
    dead_code,
    reason = "the guard payload is held for its Drop, never read"
)]
enum MemberGuard {
    Shared(OwnedRwLockReadGuard<()>),
    Exclusive(OwnedRwLockWriteGuard<()>),
}

impl AgentLocks {
    #[must_use]
    pub fn new(pool: &BTreeMap<AgentId, AgentConfig>) -> AgentLocks {
        AgentLocks {
            locks: pool
                .iter()
                .map(|(id, config)| (id.clone(), (config.concurrency, Arc::new(RwLock::new(())))))
                .collect(),
        }
    }

    /// Acquires `members`' locks in sorted id order (`BTreeSet` iteration
    /// order *is* the sort). Waits as long as it takes; callers that must
    /// abandon the wait (shutdown) race this future against their signal.
    ///
    /// A member without a lock is skipped with an error log: freeze
    /// validation guarantees flow members exist in the pool, so a miss is a
    /// `CoreTempo` bug, and skipping degrades to less serialization rather
    /// than a wedged scheduler.
    #[must_use = "a statement-position acquire takes every member lock and releases it \
                  on the spot, serializing nothing"]
    pub async fn acquire(&self, members: &BTreeSet<AgentId>) -> MemberGuards {
        let mut guards = Vec::with_capacity(members.len());
        for member in members {
            let Some((mode, lock)) = self.locks.get(member) else {
                tracing::error!(
                    agent = %member.0,
                    "no lock for a flow member; validation should have caught this"
                );
                continue;
            };
            let guard = match mode {
                AgentConcurrency::Shared => {
                    MemberGuard::Shared(Arc::clone(lock).read_owned().await)
                }
                AgentConcurrency::Exclusive => {
                    MemberGuard::Exclusive(Arc::clone(lock).write_owned().await)
                }
            };
            guards.push(guard);
        }
        MemberGuards { _guards: guards }
    }
}
