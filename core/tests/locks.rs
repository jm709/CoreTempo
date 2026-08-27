//! Per-agent readers/writers locks (multi-flow spec §4–5).
//!
//! `clippy.toml` allows `unwrap`/`expect`/`panic` inside test functions, and no
//! test here returns `Result`, so the usual file-level `#![expect(...)]` pair
//! would itself be an unfulfilled expectation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use coretempo_core::locks::AgentLocks;
use coretempo_core::types::config::{AgentConcurrency, AgentConfig, FrozenWorkflow};
use coretempo_core::types::id::{AgentId, FlowName};

fn agent(id: &str) -> AgentId {
    AgentId(id.to_string())
}

fn pool(agents: &[(&str, AgentConcurrency)]) -> BTreeMap<AgentId, AgentConfig> {
    agents
        .iter()
        .map(|(id, concurrency)| {
            (
                agent(id),
                AgentConfig {
                    concurrency: *concurrency,
                    ..AgentConfig::new("/tmp".into(), "p")
                },
            )
        })
        .collect()
}

fn members(ids: &[&str]) -> BTreeSet<AgentId> {
    ids.iter().map(|id| agent(id)).collect()
}

/// True if `fut` resolves within 50ms — "acquirable right now".
async fn acquires<F: std::future::Future>(fut: F) -> bool {
    tokio::time::timeout(Duration::from_millis(50), fut)
        .await
        .is_ok()
}

/// Two flows over the same two agents, declaring them in opposite orders.
///
/// A `tempo.toml`'s `agents = [...]` is the only unsorted member input the
/// scheduler ever sees: freezing collects it into the `BTreeSet` `acquire`
/// takes. Building the sets by hand would erase the reversal before the code
/// under test could get it wrong, so these come through the real freeze.
const REVERSED: &str = "[workflow]\nname = \"locks\"\n\
    [agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n\
    [agents.b]\ndir = \"/tmp\"\nprompt = \"p\"\n\
    [flows.forward]\nagents = [\"a\", \"b\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"a\", kind = \"ask\" } }\n\
    [flows.backward]\nagents = [\"b\", \"a\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"b\", kind = \"ask\" } }\n";

fn frozen_reversed(name: &str) -> anyhow::Result<FrozenWorkflow> {
    let path = std::env::temp_dir().join(format!(
        "coretempo-locks-{}-{name}.toml",
        std::process::id()
    ));
    std::fs::write(&path, REVERSED)?;
    let (_, frozen) = coretempo_core::workflow::load_workflow(&path)?;
    Ok(frozen)
}

/// One frozen flow's member set.
fn flow_members(frozen: &FrozenWorkflow, flow: &str) -> BTreeSet<AgentId> {
    frozen.flows[&FlowName(flow.to_string())].members.clone()
}

#[tokio::test]
async fn an_exclusive_member_serializes_overlapping_acquisitions() {
    let locks = AgentLocks::new(&pool(&[
        ("writer", AgentConcurrency::Exclusive),
        ("other", AgentConcurrency::Exclusive),
    ]));
    let held = locks.acquire(&members(&["writer"])).await;
    assert!(
        !acquires(locks.acquire(&members(&["writer"]))).await,
        "a second exclusive acquisition must block"
    );
    assert!(
        acquires(locks.acquire(&members(&["other"]))).await,
        "a disjoint member set must not block"
    );
    drop(held);
    assert!(
        acquires(locks.acquire(&members(&["writer"]))).await,
        "dropping the guards must release the lock"
    );
}

#[tokio::test]
async fn a_shared_member_overlaps_while_an_exclusive_one_in_the_set_serializes() {
    // An agent's mode is fixed when the table is built, so a read and a write
    // acquisition never queue on the *same* lock — the contention that does
    // exist is a member set holding both kinds. Here two flows span
    // {reader, writer} and a third spans {reader} alone.
    let locks = AgentLocks::new(&pool(&[
        ("reader", AgentConcurrency::Shared),
        ("writer", AgentConcurrency::Exclusive),
    ]));
    let first = locks.acquire(&members(&["reader"])).await;
    assert!(
        acquires(locks.acquire(&members(&["reader"]))).await,
        "two shared acquisitions overlap"
    );
    drop(first);

    let held = locks.acquire(&members(&["reader", "writer"])).await;
    assert!(
        !acquires(locks.acquire(&members(&["reader", "writer"]))).await,
        "one exclusive member serializes the whole set, shared members and all"
    );
    // Park a second one on the writer for real, then check the shared member is
    // not collateral damage: a flow that wants only `reader` still gets it.
    let locks = Arc::new(locks);
    let parked = tokio::spawn({
        let locks = Arc::clone(&locks);
        async move { locks.acquire(&members(&["reader", "writer"])).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        acquires(locks.acquire(&members(&["reader"]))).await,
        "a reader-only flow must not queue behind the blocked writer"
    );
    // The writer that yielded gets its turn once the holder lets go.
    drop(held);
    let guards = tokio::time::timeout(Duration::from_secs(5), parked)
        .await
        .expect("the parked writer never woke")
        .unwrap();
    assert!(
        !acquires(locks.acquire(&members(&["writer"]))).await,
        "the woken writer holds it now"
    );
    drop(guards);
    assert!(acquires(locks.acquire(&members(&["writer"]))).await);
}

#[tokio::test]
async fn guards_move_into_a_spawned_task_and_release_there() {
    // Serve workers hand their guards to an un-awaited run task, so the guards
    // must be `Send + 'static` and must release wherever they finally drop.
    fn assert_send_static<T: Send + 'static>(_value: &T) {}

    let locks = AgentLocks::new(&pool(&[("a", AgentConcurrency::Exclusive)]));
    let held = locks.acquire(&members(&["a"])).await;
    assert_send_static(&held);
    let task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(held);
    });
    task.await.unwrap();
    assert!(
        acquires(locks.acquire(&members(&["a"]))).await,
        "guards dropped inside the spawned task must release the lock"
    );
}

#[tokio::test]
async fn a_backwards_declared_flow_still_acquires_in_sorted_order() {
    // Hold 'b', then park an acquisition of the flow that declares
    // `agents = ["b", "a"]`. Sorted order means it takes 'a' before it parks on
    // 'b', so 'a' must be unavailable while it waits; walking the members in
    // the order they were declared would leave it parked holding nothing.
    let frozen = frozen_reversed("sorted").unwrap();
    let backward = flow_members(&frozen, "backward");
    assert_eq!(
        backward.iter().next(),
        Some(&agent("a")),
        "the reversed declaration must survive as a set that starts at 'a'"
    );
    let locks = Arc::new(AgentLocks::new(&frozen.agents));
    let held_b = locks.acquire(&members(&["b"])).await;
    let parked = tokio::spawn({
        let locks = Arc::clone(&locks);
        async move { locks.acquire(&backward).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !acquires(locks.acquire(&members(&["a"]))).await,
        "the parked acquisition must already hold the first member in sort order"
    );

    drop(held_b);
    let guards = tokio::time::timeout(Duration::from_secs(5), parked)
        .await
        .expect("the parked acquisition never completed")
        .unwrap();
    drop(guards);
    assert!(acquires(locks.acquire(&members(&["a", "b"]))).await);
}

#[tokio::test]
async fn overlapping_member_sets_acquired_concurrently_never_deadlock() {
    // The same two flows racing: whatever order their `agents = [...]` was
    // written in, both acquire a-then-b, so one always wins outright. 200 rounds
    // would deadlock fast if the order ever followed the declaration instead.
    let frozen = frozen_reversed("deadlock").unwrap();
    let forward = flow_members(&frozen, "forward");
    let backward = flow_members(&frozen, "backward");
    let locks = Arc::new(AgentLocks::new(&frozen.agents));
    for _ in 0..200 {
        let (l1, l2) = (Arc::clone(&locks), Arc::clone(&locks));
        let (one, two) = (forward.clone(), backward.clone());
        let t1 = tokio::spawn(async move { drop(l1.acquire(&one).await) });
        let t2 = tokio::spawn(async move { drop(l2.acquire(&two).await) });
        tokio::time::timeout(Duration::from_secs(5), async {
            t1.await.unwrap();
            t2.await.unwrap();
        })
        .await
        .expect("deadlocked");
    }
}
