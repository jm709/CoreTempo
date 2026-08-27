//! SSE endpoints (contract §6): `/v1/events` control-plane stream with ~1024-event replay
//! ring, filters, and per-consumer `bus.reset`; Task 8 adds `/v1/agents/{id}/pty`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::api::{ApiContext, ApiError, unknown_agent};
use crate::bus::EventBus;
use crate::pty::{Cursor, PtyChunk};
use crate::time::Timestamp;
use crate::types::AgentId;
use crate::types::event::{Event, EventPayload};
use crate::types::message::Origin;

pub(crate) const KEEP_ALIVE: Duration = Duration::from_secs(15);

pub(crate) fn event_type(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::RunStarted { .. } => "run.started",
        EventPayload::AgentStateChanged { .. } => "agent.state",
        EventPayload::AgentLifecycle { .. } => "agent.lifecycle",
        EventPayload::AgentNudged { .. } => "agent.nudged",
        EventPayload::AgentStalled { .. } => "agent.stalled",
        EventPayload::AgentBlocked { .. } => "agent.blocked",
        EventPayload::AgentPermissionRefused { .. } => "agent.permission_refused",
        EventPayload::MessageCreated { .. } => "message.created",
        EventPayload::MessageStatusChanged { .. } => "message.status",
        EventPayload::ReplyRejected { .. } => "reply.rejected",
        EventPayload::BusReset {} => "bus.reset",
        EventPayload::WorkflowCompleted { .. } => "workflow.completed",
        EventPayload::SessionCreated { .. } => "session.created",
        EventPayload::SessionStopped { .. } => "session.stopped",
        EventPayload::SessionResumed { .. } => "session.resumed",
        EventPayload::SessionDeleted { .. } => "session.deleted",
        EventPayload::ProjectRegistered { .. } => "project.registered",
        EventPayload::ProjectForgotten { .. } => "project.forgotten",
    }
}

/// `?types=` (trailing-`*` glob) and `?agent=` filters. `run.started` and `bus.reset`
/// always pass (contract §6.1).
pub(crate) struct EventFilterSpec {
    types: Option<Vec<String>>,
    agent: Option<AgentId>,
}

impl EventFilterSpec {
    pub(crate) fn parse(types: Option<&str>, agent: Option<&str>) -> EventFilterSpec {
        EventFilterSpec {
            types: types.map(|t| {
                t.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }),
            agent: agent.map(|a| AgentId(a.to_string())),
        }
    }

    pub(crate) fn matches(&self, event: &Event) -> bool {
        let always = match &event.payload {
            EventPayload::RunStarted { .. }
            | EventPayload::BusReset {}
            | EventPayload::WorkflowCompleted { .. }
            | EventPayload::ProjectRegistered { .. }
            | EventPayload::ProjectForgotten { .. } => true,
            EventPayload::AgentStateChanged { .. }
            | EventPayload::AgentLifecycle { .. }
            | EventPayload::AgentNudged { .. }
            | EventPayload::AgentStalled { .. }
            | EventPayload::AgentBlocked { .. }
            | EventPayload::AgentPermissionRefused { .. }
            | EventPayload::MessageCreated { .. }
            | EventPayload::MessageStatusChanged { .. }
            | EventPayload::ReplyRejected { .. }
            | EventPayload::SessionCreated { .. }
            | EventPayload::SessionStopped { .. }
            | EventPayload::SessionResumed { .. }
            | EventPayload::SessionDeleted { .. } => false,
        };
        if always {
            return true;
        }
        self.type_matches(event_type(&event.payload)) && self.agent_matches(&event.payload)
    }

    fn type_matches(&self, ty: &str) -> bool {
        let Some(patterns) = &self.types else {
            return true;
        };
        patterns.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => ty.starts_with(prefix),
            None => ty == p,
        })
    }

    fn agent_matches(&self, payload: &EventPayload) -> bool {
        let Some(want) = &self.agent else {
            return true;
        };
        match payload {
            EventPayload::AgentStateChanged { agent, .. }
            | EventPayload::AgentLifecycle { agent, .. }
            | EventPayload::AgentNudged { agent }
            | EventPayload::AgentStalled { agent }
            | EventPayload::AgentBlocked { agent, .. }
            | EventPayload::AgentPermissionRefused { agent, .. }
            | EventPayload::ReplyRejected { agent, .. }
            | EventPayload::SessionCreated { agent }
            | EventPayload::SessionStopped { agent }
            | EventPayload::SessionResumed { agent, .. }
            | EventPayload::SessionDeleted { agent } => agent == want,
            EventPayload::MessageCreated { message }
            | EventPayload::MessageStatusChanged { message } => {
                if message.to == *want {
                    return true;
                }
                match &message.from {
                    Origin::Agent(a) => a == want,
                    Origin::User | Origin::Http(_) | Origin::Trigger(_) => false,
                }
            }
            EventPayload::RunStarted { .. }
            | EventPayload::BusReset {}
            | EventPayload::WorkflowCompleted { .. }
            | EventPayload::ProjectRegistered { .. }
            | EventPayload::ProjectForgotten { .. } => true,
        }
    }
}

/// `Last-Event-ID` header wins over `?since=` (browsers set it on auto-reconnect).
pub(crate) fn resume_point(
    headers: &HeaderMap,
    params: &HashMap<String, String>,
) -> Result<Option<u64>, ApiError> {
    let raw = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| params.get("since").cloned());
    match raw {
        None => Ok(None),
        Some(raw) => raw.parse::<u64>().map(Some).map_err(|_| {
            ApiError::invalid(format!(
                "resume position '{raw}' is invalid; Last-Event-ID / since must be an integer: \
                 the seq of the last event you received (events), or the byte cursor to resume \
                 at (pty) — the last `id:` you saw, which is that chunk's start + len"
            ))
        }),
    }
}

fn bus_reset(bus: &EventBus) -> Event {
    Event {
        seq: bus.last_seq(),
        ts: Timestamp::now(),
        payload: EventPayload::BusReset {},
    }
}

fn to_sse(event: &Event) -> Option<SseEvent> {
    match serde_json::to_string(event) {
        Ok(data) => Some(
            SseEvent::default()
                .id(event.seq.to_string())
                .event(event_type(&event.payload))
                .data(data),
        ),
        Err(error) => {
            tracing::error!(%error, seq = event.seq, "failed to serialize event");
            None
        }
    }
}

async fn forward(tx: &mpsc::Sender<SseEvent>, event: &Event) -> Result<(), ()> {
    match to_sse(event) {
        Some(sse) => tx.send(sse).await.map_err(|_| ()),
        None => Ok(()),
    }
}

async fn pump_events(
    bus: EventBus,
    mut live: broadcast::Receiver<Event>,
    replay: Option<Vec<Event>>,
    filter: EventFilterSpec,
    tx: mpsc::Sender<SseEvent>,
) {
    let mut last_seq = 0u64;
    if let Some(events) = replay {
        for event in events {
            last_seq = event.seq;
            if filter.matches(&event) && forward(&tx, &event).await.is_err() {
                return;
            }
        }
    } else {
        let reset = bus_reset(&bus);
        last_seq = reset.seq;
        if forward(&tx, &reset).await.is_err() {
            return;
        }
    }
    loop {
        match live.recv().await {
            Ok(event) => {
                if event.seq <= last_seq {
                    continue; // duplicate across the replay/live seam
                }
                last_seq = event.seq;
                if filter.matches(&event) && forward(&tx, &event).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let reset = bus_reset(&bus);
                last_seq = last_seq.max(reset.seq);
                if forward(&tx, &reset).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

pub(crate) fn sse_response(rx: mpsc::Receiver<SseEvent>) -> Response {
    let stream = ReceiverStream::new(rx).map(Ok::<SseEvent, Infallible>);
    let sse = Sse::new(stream).keep_alive(KeepAlive::new().interval(KEEP_ALIVE).text("keep-alive"));
    (
        [(
            HeaderName::from_static("x-accel-buffering"),
            HeaderValue::from_static("no"),
        )],
        sse,
    )
        .into_response()
}

pub(crate) async fn events(
    State(ctx): State<ApiContext>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let since = resume_point(&headers, &params)?;
    let filter = EventFilterSpec::parse(
        params.get("types").map(String::as_str),
        params.get("agent").map(String::as_str),
    );
    let live = ctx.bus.subscribe(); // subscribe BEFORE replay: no gap at the seam
    let replay = match since {
        None => Some(Vec::new()),
        Some(s) => ctx.bus.replay_since(s),
    };
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(pump_events(ctx.bus.clone(), live, replay, filter, tx));
    Ok(sse_response(rx))
}

const B64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (RFC 4648, padded). Hand-rolled: ~20 lines beats a new dependency,
/// and split escape sequences / partial UTF-8 must survive transit byte-exact.
pub(crate) fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let mut buf = [0u8; 3];
        buf[..chunk.len()].copy_from_slice(chunk);
        let n = (u32::from(buf[0]) << 16) | (u32::from(buf[1]) << 8) | u32::from(buf[2]);
        let sextets = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        out.push(char::from(B64_TABLE[sextets[0] as usize]));
        out.push(char::from(B64_TABLE[sextets[1] as usize]));
        out.push(if chunk.len() > 1 {
            char::from(B64_TABLE[sextets[2] as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(B64_TABLE[sextets[3] as usize])
        } else {
            '='
        });
    }
    out
}

#[derive(Serialize)]
struct PtyEventData {
    seq: u64,
    b64: String,
}

/// `seq` and `id:` answer different questions: `seq` is where the chunk starts,
/// `id:` is where the next one does (`start + len`), because `Last-Event-ID`
/// says "I have everything up to here" — mirroring `seq` would replay this
/// chunk on every resume.
async fn pump_pty(mut rx: mpsc::Receiver<PtyChunk>, tx: mpsc::Sender<SseEvent>) {
    while let Some(chunk) = rx.recv().await {
        let resume_at = chunk.start.0 + chunk.bytes.len() as u64;
        let data = PtyEventData {
            seq: chunk.start.0,
            b64: b64_encode(&chunk.bytes),
        };
        let Ok(json) = serde_json::to_string(&data) else {
            continue;
        };
        let sse = SseEvent::default()
            .id(resume_at.to_string())
            .event("pty")
            .data(json);
        if tx.send(sse).await.is_err() {
            return; // client hung up
        }
    }
}

/// `GET /v1/agents/{id}/pty`: base64 raw-chunk SSE with ring-buffer replay via byte
/// cursors. Clients detect aged-out data by `first seq > since`; no reset event here.
/// `Last-Event-ID` and `?since=` take the same value — the cursor to resume at — so
/// either resumes byte-exactly (contract amendment 40).
pub(crate) async fn agent_pty(
    State(ctx): State<ApiContext>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = AgentId(id);
    if !ctx.workflow.agents.contains_key(&id) {
        return Err(unknown_agent(&ctx.workflow, &id));
    }
    let since = resume_point(&headers, &params)?.map(Cursor);
    let chunks = ctx
        .pty
        .subscribe_output(&id, since)
        .map_err(ApiError::internal)?;
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(pump_pty(chunks, tx));
    Ok(sse_response(rx))
}

#[cfg(test)]
mod tests {
    use crate::api::sse::{EventFilterSpec, event_type};
    use crate::time::Timestamp;
    use crate::types::event::{Event, EventPayload};
    use crate::types::message::{MessageKind, MessageStatus, Origin};
    use crate::types::{AgentId, AgentState, MessageId, RunId};

    fn ev(payload: EventPayload) -> Event {
        Event {
            seq: 1,
            ts: Timestamp::now(),
            payload,
        }
    }

    fn msg(from: Origin, to: &str) -> EventPayload {
        EventPayload::MessageCreated {
            message: crate::types::message::MessageRecord {
                id: MessageId("m-00000001".to_string()),
                kind: MessageKind::Send,
                from,
                to: AgentId(to.to_string()),
                body: "x".to_string(),
                status: MessageStatus::Queued,
                code: None,
                reply: None,
                created_at: Timestamp::now(),
                injected_at: None,
                completed_at: None,
                reason: None,
                reason_code: None,
            },
        }
    }

    #[test]
    fn glob_and_exact_type_patterns() {
        let f = EventFilterSpec::parse(Some("message.*,agent.state"), None);
        assert!(f.matches(&ev(msg(Origin::User, "builder"))));
        assert!(f.matches(&ev(EventPayload::AgentStateChanged {
            agent: AgentId("a".to_string()),
            state: AgentState::Idle
        })));
        assert!(!f.matches(&ev(EventPayload::AgentLifecycle {
            agent: AgentId("a".to_string()),
            phase: crate::types::event::LifecyclePhase::Exited,
            exit: Some(crate::types::AgentExit::Code(0))
        })));
    }

    #[test]
    fn agent_filter_matches_message_to_and_agent_from() {
        let f = EventFilterSpec::parse(None, Some("builder"));
        assert!(f.matches(&ev(msg(Origin::User, "builder"))));
        assert!(f.matches(&ev(msg(
            Origin::Agent(AgentId("builder".to_string())),
            "planner"
        ))));
        assert!(!f.matches(&ev(msg(Origin::User, "planner"))));
    }

    #[test]
    fn run_started_and_bus_reset_always_pass() {
        let f = EventFilterSpec::parse(Some("message.*"), Some("nobody"));
        assert!(f.matches(&ev(EventPayload::BusReset {})));
        assert!(f.matches(&ev(EventPayload::RunStarted {
            run_id: RunId("r-1f2e3d4c".to_string()),
            workflow_name: "w".to_string(),
            started_at: Timestamp::now()
        })));
    }

    #[test]
    fn event_type_strings_are_the_wire_names() {
        assert_eq!(event_type(&EventPayload::BusReset {}), "bus.reset");
    }

    #[test]
    fn session_events_take_the_agent_filter_and_project_events_always_pass() {
        let f = EventFilterSpec::parse(None, Some("s-1"));
        assert!(f.matches(&ev(EventPayload::SessionCreated {
            agent: AgentId("s-1".to_string())
        })));
        assert!(!f.matches(&ev(EventPayload::SessionStopped {
            agent: AgentId("s-2".to_string())
        })));
        assert!(f.matches(&ev(EventPayload::ProjectForgotten {
            project: crate::types::id::ProjectId("p-1".to_string())
        })));
        assert_eq!(
            event_type(&EventPayload::SessionResumed {
                agent: AgentId("s-1".to_string()),
                resumed: false
            }),
            "session.resumed"
        );
        assert_eq!(
            event_type(&EventPayload::ProjectRegistered {
                project: crate::types::id::ProjectId("p-1".to_string())
            }),
            "project.registered"
        );
    }

    #[test]
    fn base64_rfc4648_vectors() {
        use crate::api::sse::b64_encode;
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(b64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64_encode(b"hello "), "aGVsbG8g");
        assert_eq!(b64_encode(&[0xff, 0x00, 0x1b]), "/wAb");
    }
}
