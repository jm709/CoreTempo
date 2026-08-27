//! `/v1/flows*` and `/v1/trigger/{id}` handlers for a warm run (multi-flow
//! spec §5). A workflow that is already running answers per-flow triggers
//! against its live roster: no cold start, no queue — a second trigger to the
//! same flow while one is in flight is a conflict.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::api::messages::{parse_wait, wait_duration};
use crate::api::{ApiContext, ApiError, map_router_error, roster_list};
use crate::locks::MemberGuards;
use crate::router::FlowKickoff;
use crate::trigger::{
    PayloadError, SettleOnDrop, SettleSink, TriggerAccepted, TriggerStatus, TriggerView,
    WatchInputs, await_terminal, completion_status, read_payload, watch_completion,
    watcher_deadline,
};
use crate::types::FlowView;
use crate::types::config::{FrozenFlow, FrozenWorkflow, TriggerType};
use crate::types::id::FlowName;
use crate::types::message::Origin;

/// The declared flow names, or the wording every other flow error uses when
/// there are none.
fn declared_flows(workflow: &FrozenWorkflow) -> String {
    if workflow.flows.is_empty() {
        return "(none — this workflow declares no [flows.<name>] sections)".to_string();
    }
    workflow
        .flows
        .keys()
        .map(|n| n.0.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// 404 for a flow name this workflow does not declare.
///
/// Two branches, because the fix differs. With flows declared, the caller
/// picked the wrong name and the roster is the whole answer. With none
/// declared, the roster is empty and useless: what the caller needs is the
/// section to write, so the message carries a pasteable one, the file to paste
/// it into, and the agent ids that are legal inside it.
fn unknown_flow(workflow: &FrozenWorkflow, name: &FlowName) -> ApiError {
    let message = if workflow.flows.is_empty() {
        // The requested name, when it is a legal one, so pasting the snippet
        // makes this very call work.
        let flow = if FlowName::is_valid(&name.0) {
            name.0.as_str()
        } else {
            "main"
        };
        let agent = workflow
            .agents
            .keys()
            .next()
            .map_or("<agent>", |id| id.0.as_str());
        format!(
            "workflow '{}' declares no [flows.<name>] sections, so there is no flow \
             '{}' and nothing in it can be triggered over HTTP. Add one to {} and \
             restart the run:\n\n[flows.{flow}]\nagents = [\"{agent}\"]\n\
             trigger = {{ type = \"webhook\", edge = {{ to = \"{agent}\", kind = \"ask\" }} }}\
             \n\nagents and edge.to take agent ids from this workflow's roster: {}",
            workflow.name,
            name.0,
            workflow.source_path.display(),
            roster_list(workflow),
        )
    } else {
        format!(
            "no flow named '{}' in workflow '{}'; declared flows: {}. Fire one with \
             POST /v1/flows/{{name}}/trigger",
            name.0,
            workflow.name,
            declared_flows(workflow),
        )
    };
    ApiError::new(StatusCode::NOT_FOUND, "unknown_flow", message)
}

fn in_flight(active: &str) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "trigger_in_flight",
        format!(
            "trigger '{active}' is still running; a warm run takes one trigger per flow \
             at a time — poll GET /v1/trigger/{active} until it is completed or failed, \
             then fire again (queueing is serve mode's job)"
        ),
    )
}

/// 404 for a trigger id the hub has no record of. Public because serve mode's
/// listener answers the same endpoint without an [`ApiContext`].
#[must_use]
pub fn unknown_trigger(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "unknown_trigger",
        format!(
            "no trigger with id '{id}'; ids look like t-a3f91c2e and are returned by \
             POST /v1/flows/{{name}}/trigger — only the last 100 are kept, so an older \
             one is gone"
        ),
    )
}

impl From<PayloadError> for ApiError {
    fn from(error: PayloadError) -> ApiError {
        let (status, code) = match error {
            PayloadError::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
            PayloadError::NotUtf8(_) | PayloadError::Unreadable(_) => {
                (StatusCode::BAD_REQUEST, "invalid_request")
            }
        };
        ApiError::new(status, code, error.to_string())
    }
}

/// Fires one webhook flow against the live roster (multi-flow spec §5): a
/// per-flow in-flight 409, member locks held for the trigger's duration, and
/// a watcher scoped to the flow's members. Public because
/// [`post_flow_trigger`] is a thin wrapper over it and the desktop shell fires
/// flows through it too.
///
/// Cheap invariants are checked here, before any lock, and answer
/// synchronously: a caller should learn immediately that a flow name or
/// trigger type is wrong, rather than wait behind a lock acquisition that
/// would only fail the same way. Once the task below holds the member locks,
/// the only failures left are store errors, settled asynchronously as
/// `kickoff_rejected`. The trigger target being a member is a freeze-time
/// guarantee (`validate_flow_trigger`).
///
/// # Errors
/// 404 `unknown_flow` for an undeclared flow (naming the declared ones, or the
/// TOML to declare the first one); 400 for an `on_start` flow (it fires at
/// launch); 409 while the flow has a trigger in flight.
pub async fn fire_flow(
    ctx: ApiContext,
    name: FlowName,
    wait: Option<u64>,
    payload: String,
) -> Result<Response, ApiError> {
    let Some(flow) = ctx.workflow.flows.get(&name).cloned() else {
        return Err(unknown_flow(&ctx.workflow, &name));
    };
    if flow.trigger_type != TriggerType::Webhook {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!(
                "flow '{name}' is on_start: it fires its configured message at launch, \
                 not over HTTP — start it with `coretempod run <config> --flow {name}`, \
                 or fire it from the desktop Run tab's per-flow fire control; HTTP \
                 triggers drive webhook flows",
                name = name.0
            ),
        ));
    }
    let id = ctx
        .triggers
        .try_begin(&name)
        .map_err(|active| in_flight(&active))?;
    tracing::info!(trigger = id, flow = %name.0, "warm trigger accepted");
    spawn_trigger_task(ctx.clone(), (name, flow), id.clone(), payload);

    if let Some(secs) = wait
        && let Some(status) = await_terminal(&ctx.triggers, &id, wait_duration(secs)).await
    {
        return Ok(Json(TriggerView {
            trigger_id: id,
            status,
        })
        .into_response());
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(TriggerAccepted {
            trigger_id: id,
            position: 0,
        }),
    )
        .into_response())
}

/// The whole trigger lifecycle, off the request path: locks first (spec §5 —
/// an exclusive member busy in another flow's trigger must serialize us, not
/// interleave prompts in its one live session), then the kickoff message,
/// then the member-scoped watcher. The deadline runs from message creation,
/// after the lock wait: the caller's budget buys the workflow's work, and the
/// serialization it waits behind was already priced by the contended agent.
///
/// The trigger holds its flow's in-flight slot from acceptance, so every exit
/// path settles it: [`SettleOnDrop`] covers the ones no `finish` call can
/// reach (a panic here, a runtime torn down under the task). It is built
/// before the spawn and captured, so even a future dropped before its first
/// poll carries the guard and gives the flow back.
fn spawn_trigger_task(
    ctx: ApiContext,
    (name, flow): (FlowName, FrozenFlow),
    id: String,
    payload: String,
) {
    let settle = SettleOnDrop::new(Arc::clone(&ctx.triggers) as Arc<dyn SettleSink>, id.clone());
    tokio::spawn(async move {
        let Some(_guards) = acquire_or_stop(&ctx, &flow).await else {
            tracing::info!(
                trigger = id,
                "the run stopped while this trigger waited for its members"
            );
            settle.settle(run_stopped());
            return;
        };
        let origin_id = id.strip_prefix("t-").unwrap_or(&id).to_string();
        // Bound before creation: this kickoff repairs against its own flow's
        // contract (multi-flow spec §5), and a reply can land the moment the
        // message exists.
        if let Some(contract) = flow.output.clone() {
            ctx.router.bind_kickoff_contract(&origin_id, contract);
        }
        let kickoff = match ctx
            .router
            .create_kickoff(FlowKickoff {
                flow: name,
                from: Origin::Trigger(origin_id.clone()),
                to: flow.edge.to.clone(),
                kind: flow.edge.kind.message_kind(),
                body: payload,
            })
            .await
        {
            Ok(kickoff) => kickoff,
            Err(error) => {
                ctx.router.unbind_kickoff_contract(&origin_id);
                let error = map_router_error(error, &ctx.workflow);
                settle.settle(TriggerStatus::Failed {
                    reason: error.message,
                    reason_code: "kickoff_rejected".to_string(),
                });
                return;
            }
        };
        tracing::info!(trigger = id, message = %kickoff.id.0, "warm trigger fired");
        let deadline = watcher_deadline(ctx.workflow.ask_timeout);
        let inputs = watch_inputs_for(&ctx, &flow, deadline, id.clone());
        let completion = watch_completion(inputs, kickoff).await;
        settle.settle(completion_status(completion));
    });
}

/// Waits for the flow's member locks, abandoning the wait if the run starts
/// stopping first (`None`). Biased so a run already stopping never takes the
/// locks: past `Run::stop` the PTY manager is dead and the kickoff this would
/// go on to create could only be typed into nothing.
///
/// A dropped sender resolves the same way — the run that owned it is gone.
async fn acquire_or_stop(ctx: &ApiContext, flow: &FrozenFlow) -> Option<MemberGuards> {
    let mut stopping = ctx.stopping.clone();
    tokio::select! {
        biased;
        _ = stopping.wait_for(|stopping| *stopping) => None,
        guards = ctx.agent_locks.acquire(&flow.members) => Some(guards),
    }
}

/// Failure recorded for a trigger the run stopped out from under.
fn run_stopped() -> TriggerStatus {
    TriggerStatus::Failed {
        reason: "the run stopped while this trigger waited for its flow's agents to \
                 come free; no kickoff was sent — start the workflow again and fire \
                 the trigger once it is up"
            .to_string(),
        reason_code: "run_stopped".to_string(),
    }
}

/// Watcher inputs scoped to one flow's members and contract.
fn watch_inputs_for(
    ctx: &ApiContext,
    flow: &FrozenFlow,
    deadline: Duration,
    id: String,
) -> WatchInputs {
    WatchInputs {
        bus: ctx.core.bus.clone(),
        router: Arc::clone(&ctx.router),
        pty: Arc::clone(&ctx.core.pty),
        roster: flow.members.iter().cloned().collect(),
        idle_debounce: ctx.workflow.idle_debounce,
        deadline,
        output: flow.output.clone(),
        trigger_id: Some(id),
    }
}

/// Fires the named webhook flow against the live roster. Any content type is
/// accepted (the body is the kickoff message verbatim); `?wait=<secs>`
/// long-polls for the result.
pub(crate) async fn post_flow_trigger(
    State(ctx): State<ApiContext>,
    Path(name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    body: Body,
) -> Result<Response, ApiError> {
    let wait = parse_wait(params.get("wait").map(String::as_str))?;
    // Read the payload before claiming the flow: a rejected body must not burn
    // the flow's in-flight slot.
    let payload = read_payload(body).await?;
    fire_flow(ctx, FlowName(name), wait, payload).await
}

/// `GET /v1/flows`: every declared flow with its live counters (multi-flow
/// spec §5). Warm runs have no queue, so depth is 0 and `running` is the flow's
/// in-flight trigger.
pub(crate) async fn list_flows(State(ctx): State<ApiContext>) -> Json<Vec<FlowView>> {
    Json(
        ctx.workflow
            .flows
            .iter()
            .map(|(name, flow)| FlowView {
                name: name.clone(),
                trigger_type: flow.trigger_type,
                target: flow.edge.to.clone(),
                queue_depth: 0,
                running: usize::from(ctx.triggers.in_flight(name).is_some()),
            })
            .collect(),
    )
}

/// Bare `POST /v1/trigger` was removed (multi-flow spec §5); the 404 names the
/// declared flows and the per-flow route so an old caller can rewrite itself.
pub(crate) async fn removed_trigger_route(State(ctx): State<ApiContext>) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "invalid_request",
        format!(
            "POST /v1/trigger was replaced by POST /v1/flows/{{name}}/trigger; \
             declared flows: {}. @coretempo/client 2.x targets the new route",
            declared_flows(&ctx.workflow),
        ),
    )
}

pub(crate) async fn get_trigger(
    State(ctx): State<ApiContext>,
    Path(id): Path<String>,
) -> Result<Json<TriggerView>, ApiError> {
    let status = ctx.triggers.get(&id).ok_or_else(|| unknown_trigger(&id))?;
    Ok(Json(TriggerView {
        trigger_id: id,
        status,
    }))
}
