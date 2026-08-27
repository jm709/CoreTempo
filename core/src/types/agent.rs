//! Agent state and roster types (contracts §2.3).

use serde::{Deserialize, Serialize};

use crate::types::id::AgentId;

/// Wire strings: "starting" "idle" "working" "exited" "restarting".
/// (`exited` is the API name; the UI labels it "dead".)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Starting,
    Idle,
    Working,
    Exited,
    Restarting,
}

/// How the last session ended. `{"code": 3}` for a normal exit; `{"signal":
/// "Terminated"}` when a signal killed it, named as `strsignal(3)` describes
/// it (portable-pty reports a signalled child as code 1 plus the name, and
/// the code alone read as a real exit status — #90).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExit {
    Code(i32),
    Signal(String),
}

/// `GET /v1/agents` element. `state` is the RAW (undebounced) state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: AgentId,
    pub state: AgentState,
    /// Asks SENT BY this agent, not yet terminal. Not asks addressed to it: an
    /// agent holding an unanswered kickoff still reads `0` here.
    pub pending_asks: u64,
    /// Set only when `state == exited`.
    pub exit: Option<AgentExit>,
    /// The agent's `PermissionRequest` hook fired and nothing has cleared it yet
    /// (spec 2026-08-17 §3).
    #[serde(default)]
    pub blocked: bool,
}

/// `GET /v1/agents/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDetail {
    #[serde(flatten)]
    pub info: AgentInfo,
    /// Frozen, `~`-expanded working directory.
    pub dir: String,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub auto_clear: bool,
    /// Spawned against a CoreTempo-managed `CLAUDE_CONFIG_DIR` (spec 2026-08-24).
    pub isolated_config: bool,
    /// Frozen, absolute skill directories linked into that dir.
    pub skills: Vec<String>,
    /// Current end-of-stream byte cursor.
    pub pty_cursor: u64,
}

#[cfg(test)]
mod tests {
    use crate::types::agent::{AgentDetail, AgentInfo, AgentState};
    use crate::types::id::AgentId;

    #[test]
    fn agent_state_wire_strings() {
        let states = [
            (AgentState::Starting, "\"starting\""),
            (AgentState::Idle, "\"idle\""),
            (AgentState::Working, "\"working\""),
            (AgentState::Exited, "\"exited\""),
            (AgentState::Restarting, "\"restarting\""),
        ];
        for (state, wire) in states {
            assert_eq!(serde_json::to_string(&state).unwrap(), wire);
        }
    }

    #[test]
    fn detail_flattens_info() {
        let detail = AgentDetail {
            info: AgentInfo {
                id: AgentId("builder".into()),
                state: AgentState::Idle,
                pending_asks: 2,
                exit: None,
                blocked: false,
            },
            dir: "/home/u/proj".into(),
            model: None,
            permission_mode: Some("acceptEdits".into()),
            auto_clear: true,
            isolated_config: false,
            skills: Vec::new(),
            pty_cursor: 4096,
        };
        let v = serde_json::to_value(&detail).unwrap();
        assert_eq!(v.get("id").unwrap(), "builder");
        assert_eq!(v.get("pending_asks").unwrap(), 2);
        assert_eq!(v.get("pty_cursor").unwrap(), 4096);
        assert_eq!(v.get("blocked").unwrap(), false);
        assert!(v.get("info").is_none(), "info must be flattened");
    }
}
