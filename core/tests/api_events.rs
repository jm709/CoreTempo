#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

mod support;

use coretempo_core::types::event::EventPayload;
use coretempo_core::types::{AgentId, AgentState};

fn state_event(agent: &str, state: AgentState) -> EventPayload {
    EventPayload::AgentStateChanged {
        agent: AgentId(agent.to_string()),
        state,
    }
}

#[test]
fn live_events_carry_seq_id_and_type() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let mut sse = srv.open_sse("/v1/events", None)?;
    srv.handles
        .bus
        .publish(state_event("builder", AgentState::Working));
    let ev = sse.next_event()?;
    assert_eq!(ev.event.as_deref(), Some("agent.state"));
    assert_eq!(ev.id.as_deref(), Some("1"));
    let data: serde_json::Value = serde_json::from_str(&ev.data)?;
    assert_eq!(data["seq"], 1);
    assert_eq!(data["type"], "agent.state");
    assert_eq!(data["agent"], "builder");
    assert_eq!(data["state"], "working");
    Ok(())
}

#[test]
fn since_and_last_event_id_replay_forward() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    srv.handles
        .bus
        .publish(state_event("builder", AgentState::Working));
    srv.handles
        .bus
        .publish(state_event("builder", AgentState::Idle));
    srv.handles
        .bus
        .publish(state_event("planner", AgentState::Working));
    let mut sse = srv.open_sse("/v1/events?since=1", None)?;
    assert_eq!(sse.next_event()?.id.as_deref(), Some("2"));
    assert_eq!(sse.next_event()?.id.as_deref(), Some("3"));
    let mut sse = srv.open_sse("/v1/events", Some("2"))?;
    assert_eq!(sse.next_event()?.id.as_deref(), Some("3"));
    Ok(())
}

#[test]
fn aged_out_replay_yields_bus_reset() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    for _ in 0..1100 {
        srv.handles
            .bus
            .publish(state_event("builder", AgentState::Working));
    }
    let mut sse = srv.open_sse("/v1/events?since=1", None)?;
    let ev = sse.next_event()?;
    assert_eq!(ev.event.as_deref(), Some("bus.reset"));
    let data: serde_json::Value = serde_json::from_str(&ev.data)?;
    assert_eq!(data["type"], "bus.reset");
    assert_eq!(data["seq"], 1100);
    // and the stream continues live after the reset
    srv.handles
        .bus
        .publish(state_event("planner", AgentState::Idle));
    assert_eq!(sse.next_event()?.id.as_deref(), Some("1101"));
    Ok(())
}

#[test]
fn type_and_agent_filters_apply() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let mut sse = srv.open_sse("/v1/events?types=agent.*&agent=builder", None)?;
    srv.handles
        .bus
        .publish(state_event("planner", AgentState::Working)); // filtered: agent
    srv.handles
        .bus
        .publish(state_event("builder", AgentState::Working)); // passes
    let ev = sse.next_event()?;
    let data: serde_json::Value = serde_json::from_str(&ev.data)?;
    assert_eq!(data["agent"], "builder");
    Ok(())
}
