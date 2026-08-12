//! Control-plane event enum (contracts §2.4). PTY bytes never ride these events.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::time::Timestamp;
use crate::types::agent::AgentState;
use crate::types::id::{AgentId, MessageId, RunId};
use crate::types::message::MessageRecord;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Monotonic per run, starts at 1. Assigned solely by `EventBus::publish`.
    pub seq: u64,
    pub ts: Timestamp,
    #[serde(flatten)]
    pub payload: EventPayload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventPayload {
    #[serde(rename = "run.started")]
    RunStarted {
        run_id: RunId,
        workflow_name: String,
        started_at: Timestamp,
    },

    /// RAW transitions (UI shows truth); the debounced signal is internal-only.
    #[serde(rename = "agent.state")]
    AgentStateChanged { agent: AgentId, state: AgentState },

    #[serde(rename = "agent.lifecycle")]
    AgentLifecycle {
        agent: AgentId,
        phase: LifecyclePhase,
        exit_code: Option<i32>,
    },

    /// Obligation nudge injected (spec §2): the agent idled with unmet steps.
    #[serde(rename = "agent.nudged")]
    AgentNudged { agent: AgentId },

    /// Idle again after its one nudge — un-cleared and waiting for a human.
    #[serde(rename = "agent.stalled")]
    AgentStalled { agent: AgentId },

    /// Fat event: full record snapshot.
    #[serde(rename = "message.created")]
    MessageCreated { message: MessageRecord },

    /// Fat event: full record snapshot.
    #[serde(rename = "message.status")]
    MessageStatusChanged { message: MessageRecord },

    /// Synthesized per-consumer on replay gap / `broadcast::Lagged`; never published.
    #[serde(rename = "bus.reset")]
    BusReset {},

    /// A reply failed output-schema validation and was rejected for repair
    /// (design 2026-08-06). Published once per rejection.
    #[serde(rename = "reply.rejected")]
    ReplyRejected {
        message: MessageId,
        agent: AgentId,
        errors: String,
    },

    /// Trigger/kickoff completion (spec 2026-08-03 triggers §2). `code`/`reply`/
    /// `output` are set only for `replied`; `reason`/`reason_code` only for
    /// `failed` (a timeout is already distinguished by `result`).
    #[serde(rename = "workflow.completed")]
    WorkflowCompleted {
        result: CompletionResult,
        code: Option<u8>,
        reply: Option<String>,
        trigger_id: Option<String>,
        message: MessageId,
        output: Option<Value>,
        reason: Option<String>,
        reason_code: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    Spawned,
    Exited,
    Restarting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionResult {
    Replied,
    Quiesced,
    Failed,
    Timeout,
}

#[cfg(test)]
mod tests {
    use crate::time::Timestamp;
    use crate::types::agent::AgentState;
    use crate::types::event::{CompletionResult, Event, EventPayload, LifecyclePhase};
    use crate::types::id::{AgentId, MessageId, RunId};

    #[test]
    fn run_started_wire_form() {
        let ev = Event {
            seq: 1,
            ts: Timestamp("2026-08-01T17:00:00Z".into()),
            payload: EventPayload::RunStarted {
                run_id: RunId("r-1f2e3d4c".into()),
                workflow_name: "core-tempo-dev".into(),
                started_at: Timestamp("2026-08-01T17:00:00Z".into()),
            },
        };
        let expected = serde_json::json!({
            "seq": 1, "ts": "2026-08-01T17:00:00Z", "type": "run.started",
            "run_id": "r-1f2e3d4c", "workflow_name": "core-tempo-dev",
            "started_at": "2026-08-01T17:00:00Z"
        });
        assert_eq!(serde_json::to_value(&ev).unwrap(), expected);
    }

    #[test]
    fn agent_state_wire_form() {
        let ev = Event {
            seq: 6,
            ts: Timestamp("2026-08-01T17:00:05Z".into()),
            payload: EventPayload::AgentStateChanged {
                agent: AgentId("builder".into()),
                state: AgentState::Working,
            },
        };
        let expected = serde_json::json!({
            "seq": 6, "ts": "2026-08-01T17:00:05Z", "type": "agent.state",
            "agent": "builder", "state": "working"
        });
        assert_eq!(serde_json::to_value(&ev).unwrap(), expected);
        let back: Event = serde_json::from_value(expected).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn lifecycle_and_reset_wire_forms() {
        let ev = Event {
            seq: 7,
            ts: Timestamp("2026-08-01T17:00:06Z".into()),
            payload: EventPayload::AgentLifecycle {
                agent: AgentId("docs".into()),
                phase: LifecyclePhase::Exited,
                exit_code: Some(1),
            },
        };
        let expected = serde_json::json!({
            "seq": 7, "ts": "2026-08-01T17:00:06Z", "type": "agent.lifecycle",
            "agent": "docs", "phase": "exited", "exit_code": 1
        });
        assert_eq!(serde_json::to_value(&ev).unwrap(), expected);

        let reset = Event {
            seq: 41,
            ts: Timestamp("2026-08-01T17:00:41Z".into()),
            payload: EventPayload::BusReset {},
        };
        assert_eq!(
            serde_json::to_value(&reset).unwrap(),
            serde_json::json!({"seq": 41, "ts": "2026-08-01T17:00:41Z", "type": "bus.reset"})
        );
    }

    #[test]
    fn nudged_and_stalled_wire_forms() {
        let nudged = Event {
            seq: 9,
            ts: Timestamp("2026-08-03T10:00:00Z".into()),
            payload: EventPayload::AgentNudged {
                agent: AgentId("planner".into()),
            },
        };
        assert_eq!(
            serde_json::to_value(&nudged).unwrap(),
            serde_json::json!({
                "seq": 9, "ts": "2026-08-03T10:00:00Z",
                "type": "agent.nudged", "agent": "planner"
            })
        );
        let stalled = Event {
            seq: 10,
            ts: Timestamp("2026-08-03T10:00:05Z".into()),
            payload: EventPayload::AgentStalled {
                agent: AgentId("planner".into()),
            },
        };
        let json = serde_json::to_value(&stalled).unwrap();
        assert_eq!(json["type"], "agent.stalled");
        let back: Event = serde_json::from_value(json).unwrap();
        assert_eq!(back, stalled);
    }

    #[test]
    fn reply_rejected_wire_form() {
        let ev = Event {
            seq: 20,
            ts: Timestamp("2026-08-06T10:00:00Z".into()),
            payload: EventPayload::ReplyRejected {
                message: MessageId("m-a3f91c2e".into()),
                agent: AgentId("translator".into()),
                errors: "at /name: required".into(),
            },
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "reply.rejected");
        assert_eq!(json["message"], "m-a3f91c2e");
        let back: Event = serde_json::from_value(json).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn workflow_completed_wire_forms() {
        let replied = Event {
            seq: 12,
            ts: Timestamp("2026-08-03T20:00:00Z".into()),
            payload: EventPayload::WorkflowCompleted {
                result: CompletionResult::Replied,
                code: Some(0),
                reply: Some("{\"ok\":true}".into()),
                trigger_id: Some("t-a3f91c2e".into()),
                message: MessageId("m-b7c21d0e".into()),
                output: Some(serde_json::json!({"ok": true})),
                reason: None,
                reason_code: None,
            },
        };
        assert_eq!(
            serde_json::to_value(&replied).unwrap(),
            serde_json::json!({
                "seq": 12, "ts": "2026-08-03T20:00:00Z", "type": "workflow.completed",
                "result": "replied", "code": 0, "reply": "{\"ok\":true}",
                "trigger_id": "t-a3f91c2e", "message": "m-b7c21d0e",
                "output": {"ok": true}, "reason": null, "reason_code": null
            })
        );
        let failed = Event {
            seq: 13,
            ts: Timestamp("2026-08-03T20:00:05Z".into()),
            payload: EventPayload::WorkflowCompleted {
                result: CompletionResult::Failed,
                code: None,
                reply: None,
                trigger_id: Some("t-a3f91c2e".into()),
                message: MessageId("m-b7c21d0e".into()),
                output: None,
                reason: Some("the agent exited".into()),
                reason_code: Some("agent_exited".into()),
            },
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["reason_code"], "agent_exited");
        let back: Event = serde_json::from_value(json).unwrap();
        assert_eq!(back, failed);
    }
}
