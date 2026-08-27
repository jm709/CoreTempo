//! Wire types shared by every `CoreTempo` crate. Serde rules (frozen): `snake_case`
//! fields, explicit nulls, `deny_unknown_fields` on config structs.

pub mod agent;
pub mod api;
pub mod config;
pub mod event;
pub mod id;
pub mod message;

pub use crate::types::agent::{AgentDetail, AgentExit, AgentInfo, AgentState};
pub use crate::types::api::{
    AgentListResponse, AgentStateResponse, ApiErrorBody, ApiErrorDetail, ApiFile,
    CreateMessageRequest, FlowView, Health, MessageListResponse, ReplyRequest, ReportStateRequest,
    ReportedState, RestartResponse, RunInfo, Snapshot, WorkflowResponse,
};
#[cfg(feature = "server")]
pub use crate::types::config::FrozenFlow;
pub use crate::types::config::{
    AgentConcurrency, AgentConfig, FlowConfig, FrozenWorkflow, ResolvedServer, ServerOverrides,
    ServerSection, ValidationIssue, WorkflowFile, WorkflowSection,
};
pub use crate::types::event::{Event, EventPayload, LifecyclePhase};
pub use crate::types::id::{AgentId, FlowName, MessageId, RunId, Token};
pub use crate::types::message::{
    MessageKind, MessageRecord, MessageStatus, Origin, OriginParseError,
};
