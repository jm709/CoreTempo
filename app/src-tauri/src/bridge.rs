use coretempo_core::bus::EventBus;
use coretempo_core::time::Timestamp;
use coretempo_core::types::{Event, EventPayload};
use tauri::Emitter;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;

/// Tauri event name carrying every core `Event` (contracts §8.2). Payload is the identical
/// `Event` JSON the SSE `/v1/events` endpoint emits in `data:`.
pub const EVENT_NAME: &str = "coretempo:event";

/// Spawn the bus→webview bridge for one run. Ends when the bus closes (run stopped);
/// `run_stop` also aborts it as a belt-and-braces cleanup.
pub fn spawn_event_bridge<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    bus: EventBus,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let rx = bus.subscribe();
        forward_events(bus, rx, move |event| {
            if let Err(err) = app.emit(EVENT_NAME, event) {
                tracing::warn!(%err, "failed to emit coretempo:event to webview");
            }
        })
        .await;
    })
}

/// Synthesized per-consumer, never `publish`ed (contracts §9): seq = latest published seq.
fn bus_reset(bus: &EventBus) -> Event {
    Event {
        seq: bus.last_seq(),
        ts: Timestamp::now(),
        payload: EventPayload::BusReset {},
    }
}

/// Core forwarding loop, separated from Tauri so tests drive it with a plain closure.
/// Replays the bus ring first (the bridge spawns after `Run::start` already published
/// `run.started`), then goes live, deduping the overlap by seq. `Lagged` → `bus.reset`
/// (frontend re-snapshots). `Closed` → return.
pub(crate) async fn forward_events<F>(bus: EventBus, mut rx: Receiver<Event>, mut emit: F)
where
    F: FnMut(&Event) + Send,
{
    let mut last_seq = 0u64;
    match bus.replay_since(0) {
        Some(backlog) => {
            for event in backlog {
                last_seq = event.seq;
                emit(&event);
            }
        }
        // Aged out already (>REPLAY_RING events before the bridge started): tell the
        // frontend to snapshot instead of replaying a gap.
        None => emit(&bus_reset(&bus)),
    }
    loop {
        match rx.recv().await {
            Ok(event) => {
                if event.seq <= last_seq {
                    continue;
                }
                last_seq = event.seq;
                emit(&event);
            }
            Err(RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "event bridge lagged; emitting bus.reset");
                emit(&bus_reset(&bus));
            }
            Err(RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "tests assert inside Result-returning fns"
)]
mod tests {
    use super::*;
    use coretempo_core::types::{AgentId, AgentState};
    use std::time::Duration;

    fn state_payload(agent: &str, state: AgentState) -> EventPayload {
        EventPayload::AgentStateChanged {
            agent: AgentId(agent.to_string()),
            state,
        }
    }

    async fn next_event(
        out: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    ) -> anyhow::Result<Event> {
        tokio::time::timeout(Duration::from_secs(2), out.recv())
            .await?
            .ok_or_else(|| anyhow::anyhow!("bridge stopped before emitting expected event"))
    }

    #[tokio::test]
    async fn forwards_backlog_then_live_without_duplicates() -> anyhow::Result<()> {
        let bus = EventBus::new();
        bus.publish(state_payload("builder", AgentState::Starting));
        bus.publish(state_payload("builder", AgentState::Idle));
        let rx = bus.subscribe();
        bus.publish(state_payload("builder", AgentState::Working));

        let (tx, mut out) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let task = tokio::spawn(forward_events(bus.clone(), rx, move |event| {
            let _ = tx.send(event.clone());
        }));

        // seq 3 is in BOTH the replay ring and the live receiver's backlog — exactly once out.
        for expected in 1..=3u64 {
            assert_eq!(next_event(&mut out).await?.seq, expected);
        }
        bus.publish(state_payload("builder", AgentState::Idle));
        assert_eq!(next_event(&mut out).await?.seq, 4);
        task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn lagged_receiver_emits_bus_reset_with_latest_seq() -> anyhow::Result<()> {
        let bus = EventBus::new();
        let rx = bus.subscribe();
        let total = (EventBus::CAPACITY + EventBus::REPLAY_RING + 8) as u64;
        for _ in 0..total {
            bus.publish(state_payload("builder", AgentState::Working));
        }
        // seq 1 has aged out of the replay ring AND rx has lagged.
        let (tx, mut out) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let task = tokio::spawn(forward_events(bus.clone(), rx, move |event| {
            let _ = tx.send(event.clone());
        }));

        let first = next_event(&mut out).await?;
        assert_eq!(first.payload, EventPayload::BusReset {});
        assert_eq!(first.seq, total);
        task.abort();
        Ok(())
    }
}
