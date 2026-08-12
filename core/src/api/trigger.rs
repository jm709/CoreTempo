//! `/v1/trigger*` handlers for a warm run (spec triggers §4). A webhook workflow
//! that is already running answers triggers against its live roster: no cold
//! start, no queue — a second trigger while one is in flight is a conflict.

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
use crate::trigger::{
    PayloadError, TriggerAccepted, TriggerHub, TriggerStatus, TriggerView, WatchInputs,
    await_terminal, completion_status, read_payload, watch_completion, watcher_deadline,
};
use crate::types::config::{TriggerConfig, TriggerType};
use crate::types::message::{MessageRecord, Origin};

/// The workflow's webhook trigger, or a 404 naming the section that declares one.
fn webhook_trigger(ctx: &ApiContext) -> Result<&TriggerConfig, ApiError> {
    let declared = match ctx.workflow_file.trigger.as_ref() {
        Some(trigger) if trigger.trigger_type == TriggerType::Webhook => return Ok(trigger),
        Some(_) => "its [trigger] is an on_start trigger, which fires at launch instead",
        None => "it declares no [trigger] at all",
    };
    Err(ApiError::new(
        StatusCode::NOT_FOUND,
        "invalid_request",
        format!(
            "workflow '{}' cannot be triggered over HTTP: {declared}. Add \
             [trigger] type = \"webhook\" with edge = {{ to = \"<agent>\", kind = \"ask\" }} \
             to {} and restart the run; roster: {}",
            ctx.workflow.name,
            ctx.workflow.source_path.display(),
            roster_list(&ctx.workflow),
        ),
    ))
}

fn in_flight(active: &str) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "trigger_in_flight",
        format!(
            "trigger '{active}' is still running; a warm run takes one trigger at a time — \
             poll GET /v1/trigger/{active} until it is completed or failed, then fire again \
             (queueing is serve mode's job)"
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
             POST /v1/trigger — only the last 100 are kept, so an older one is gone"
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

/// Watcher inputs over this run's live wiring.
fn watch_inputs(ctx: &ApiContext, deadline: Duration, id: String) -> WatchInputs {
    WatchInputs {
        bus: ctx.bus.clone(),
        router: Arc::clone(&ctx.router),
        pty: Arc::clone(&ctx.pty),
        roster: ctx.workflow.agents.keys().cloned().collect(),
        idle_debounce: ctx.workflow.idle_debounce,
        deadline,
        output: ctx.workflow.output.clone(),
        trigger_id: Some(id),
    }
}

/// Watches the kickoff in the background and records the outcome, so the POST
/// answers 202 without holding the connection open for the whole workflow.
fn spawn_watcher(hub: &Arc<TriggerHub>, inputs: WatchInputs, kickoff: MessageRecord, id: String) {
    let hub = Arc::clone(hub);
    tokio::spawn(async move {
        let completion = watch_completion(inputs, kickoff).await;
        hub.finish(&id, completion_status(completion));
    });
}

/// Fires the workflow's webhook trigger. Any content type is accepted (the body
/// is the kickoff message verbatim); `?wait=<secs>` long-polls for the result.
pub(crate) async fn post_trigger(
    State(ctx): State<ApiContext>,
    Query(params): Query<HashMap<String, String>>,
    body: Body,
) -> Result<Response, ApiError> {
    let edge = webhook_trigger(&ctx)?.edge.clone();
    let wait = parse_wait(params.get("wait").map(String::as_str))?;
    // Read the payload before claiming the workflow: a rejected body must not
    // burn the one in-flight slot.
    let payload = read_payload(body).await?;
    let id = ctx
        .triggers
        .try_begin()
        .map_err(|active| in_flight(&active))?;

    let origin = Origin::Http(id.strip_prefix("t-").unwrap_or(&id).to_string());
    let kickoff = match ctx
        .router
        .create_message(origin, edge.to, edge.kind.message_kind(), payload)
        .await
    {
        Ok(kickoff) => kickoff,
        Err(error) => {
            let error = map_router_error(error, &ctx.workflow);
            ctx.triggers.finish(
                &id,
                TriggerStatus::Failed {
                    reason: error.message.clone(),
                    reason_code: "kickoff_rejected".to_string(),
                },
            );
            return Err(error);
        }
    };
    tracing::info!(trigger = id, message = %kickoff.id.0, "warm trigger fired");
    // Immediately: the watcher's deadline runs from here, and a kickoff that is
    // never injected must still terminate.
    let deadline = watcher_deadline(ctx.workflow.ask_timeout);
    spawn_watcher(
        &ctx.triggers,
        watch_inputs(&ctx, deadline, id.clone()),
        kickoff,
        id.clone(),
    );

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
