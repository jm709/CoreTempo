//! REST request/response bodies for the /v1 API (contract §5.1/§5.2).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::time::Timestamp;
use crate::trigger::TriggerView;
use crate::types::agent::{AgentDetail, AgentInfo, AgentState};
use crate::types::config::WorkflowFile;
use crate::types::id::{AgentId, RunId, Token};
use crate::types::message::{MessageKind, MessageRecord};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateMessageRequest {
    pub to: AgentId,
    pub kind: MessageKind,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplyRequest {
    pub code: u8,
    pub body: String,
}

/// The two states an agent may report about itself (`POST /v1/agents/{id}/state`).
/// Lifecycle states (`starting`, `exited`, `restarting`) are the supervisor's to set,
/// so they are not accepted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportedState {
    Working,
    Idle,
}

impl From<ReportedState> for AgentState {
    fn from(state: ReportedState) -> AgentState {
        match state {
            ReportedState::Working => AgentState::Working,
            ReportedState::Idle => AgentState::Idle,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportStateRequest {
    pub state: ReportedState,
}

/// `POST /v1/agents/{id}/state` response: the state now on the raw state channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStateResponse {
    pub agent: AgentId,
    pub state: AgentState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageListResponse {
    pub messages: Vec<MessageRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentListResponse {
    pub agents: Vec<AgentInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestartResponse {
    pub agent: AgentId,
    pub state: AgentState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowResponse {
    pub run_id: RunId,
    pub started_at: Timestamp,
    pub workflow: WorkflowFile,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    pub version: String,
    pub run_id: RunId,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiErrorBody {
    pub error: ApiErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiErrorDetail {
    pub code: String,
    pub message: String,
}

/// Contents of `~/.coretempo/runs/<run_id>/api.json` — shared by the server-side writer
/// (`api::auth::write_api_file`) and the `tempo` CLI's fallback connection resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiFile {
    pub port: u16,
    pub token: Token,
    pub run_id: RunId,
}

/// Identity of the run the desktop app currently owns (contract §8.1): returned by the
/// `run_start` command and embedded in `snapshot()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunInfo {
    pub run_id: RunId,
    pub workflow_name: String,
    /// Path the workflow was loaded from, as the user typed it.
    pub workflow_path: String,
    pub started_at: Timestamp,
    pub port: u16,
    /// Frozen `[workflow] scrollback` — terminal history depth for UI clients.
    pub scrollback: u32,
}

/// Whole-UI state in one call (contract §8.1): what the desktop app reads on start and after a
/// webview reload. `run: None` (with empty collections and `last_seq: 0`) means no active run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub run: Option<RunInfo>,
    pub agents: Vec<AgentDetail>,
    /// Most recent 200 messages, `created_at` descending.
    pub messages: Vec<MessageRecord>,
    /// Per-agent end-of-stream cursors; a reloaded terminal instead passes
    /// `subscribe_pty(since_cursor: null)` to replay the full ring tail.
    pub pty_cursors: BTreeMap<AgentId, u64>,
    /// Event dedup floor: the frontend ignores `coretempo:event` payloads with `seq <= last_seq`.
    pub last_seq: u64,
    /// Trigger history for this run, oldest first (hub-capped at 100). Empty when
    /// the workflow has fired no kickoffs.
    pub triggers: Vec<TriggerView>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::time::Timestamp;
    use crate::types::api::{
        ApiErrorBody, ApiErrorDetail, ApiFile, CreateMessageRequest, Health, ReplyRequest, RunInfo,
        Snapshot,
    };
    use crate::types::id::{AgentId, RunId, Token};
    use crate::types::message::MessageKind;

    #[test]
    fn create_message_request_round_trips() {
        let json = r#"{"to":"builder","kind":"ask","body":"hi"}"#;
        let req: CreateMessageRequest = serde_json::from_str(json).expect("parse");
        assert_eq!(req.to, AgentId("builder".to_string()));
        assert_eq!(req.kind, MessageKind::Ask);
        assert_eq!(req.body, "hi");
        assert_eq!(serde_json::to_string(&req).expect("ser"), json);
    }

    #[test]
    fn reply_request_shape() {
        let req: ReplyRequest = serde_json::from_str(r#"{"code":1,"body":"no"}"#).expect("parse");
        assert_eq!((req.code, req.body.as_str()), (1, "no"));
    }

    #[test]
    fn error_body_matches_contract_shape() {
        let body = ApiErrorBody {
            error: ApiErrorDetail {
                code: "unknown_agent".to_string(),
                message: "no agent named 'buidler'; roster: planner, builder".to_string(),
            },
        };
        let json = serde_json::to_value(&body).expect("ser");
        assert_eq!(json["error"]["code"], "unknown_agent");
        assert!(
            json["error"]["message"]
                .as_str()
                .expect("str")
                .contains("roster")
        );
    }

    #[test]
    fn health_field_names() {
        let h = Health {
            status: "ok".to_string(),
            version: "0.1.0".to_string(),
            run_id: RunId("r-1f2e3d4c".to_string()),
            uptime_secs: 7,
        };
        let json = serde_json::to_value(&h).expect("ser");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["run_id"], "r-1f2e3d4c");
        assert_eq!(json["uptime_secs"], 7);
    }

    #[test]
    fn api_file_matches_run_file_shape() {
        let f = ApiFile {
            port: 4820,
            token: Token("ab".repeat(32)),
            run_id: RunId("r-1f2e3d4c".to_string()),
        };
        let json = serde_json::to_string(&f).expect("ser");
        let back: ApiFile = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, f);
        assert!(json.contains(r#""port":4820"#));
    }

    #[test]
    fn run_info_matches_frozen_field_names() {
        let info = RunInfo {
            run_id: RunId("r-1f2e3d4c".to_string()),
            workflow_name: "ship-it".to_string(),
            workflow_path: "/home/dev/proj/tempo.toml".to_string(),
            started_at: Timestamp("2026-08-01T17:03:11Z".to_string()),
            port: 4820,
            scrollback: 5_000,
        };
        let json = serde_json::to_value(&info).expect("ser");
        assert_eq!(json["run_id"], "r-1f2e3d4c");
        assert_eq!(json["workflow_name"], "ship-it");
        assert_eq!(json["workflow_path"], "/home/dev/proj/tempo.toml");
        assert_eq!(json["started_at"], "2026-08-01T17:03:11Z");
        assert_eq!(json["port"], 4820);
        assert_eq!(json["scrollback"], 5_000);
        let back: RunInfo = serde_json::from_value(json).expect("parse");
        assert_eq!(back, info);
    }

    #[test]
    fn snapshot_matches_frozen_field_names() {
        let snapshot = Snapshot {
            run: None,
            agents: Vec::new(),
            messages: Vec::new(),
            pty_cursors: BTreeMap::from([(AgentId("builder".to_string()), 4096)]),
            last_seq: 17,
            triggers: Vec::new(),
        };
        let json = serde_json::to_value(&snapshot).expect("ser");
        assert_eq!(json["run"], serde_json::Value::Null);
        assert!(json["agents"].as_array().expect("array").is_empty());
        assert!(json["messages"].as_array().expect("array").is_empty());
        assert_eq!(json["pty_cursors"]["builder"], 4096);
        assert_eq!(json["last_seq"], 17);
        assert!(json["triggers"].as_array().expect("array").is_empty());
        let back: Snapshot = serde_json::from_value(json).expect("parse");
        assert_eq!(back, snapshot);
    }

    #[test]
    fn timestamp_reexport_compiles() {
        let _t: Timestamp = Timestamp::now();
    }
}
