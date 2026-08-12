//! `/v1/agents*` handlers: roster (raw state + `pending_asks`), detail, reported
//! state, async restart.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::api::{ApiContext, ApiError, auth, unknown_agent};
use crate::types::agent::{AgentDetail, AgentInfo, AgentState};
use crate::types::config::AgentConfig;
use crate::types::message::Origin;
use crate::types::{
    AgentId, AgentListResponse, AgentStateResponse, ReportStateRequest, RestartResponse,
};

fn roster_agent<'a>(ctx: &'a ApiContext, id: &AgentId) -> Result<&'a AgentConfig, ApiError> {
    ctx.workflow
        .agents
        .get(id)
        .ok_or_else(|| unknown_agent(&ctx.workflow, id))
}

fn agent_info(ctx: &ApiContext, id: &AgentId) -> Result<AgentInfo, ApiError> {
    let state = ctx.pty.state(id).map_err(ApiError::internal)?;
    let exit_code = ctx.pty.exit_code(id).map_err(ApiError::internal)?;
    Ok(AgentInfo {
        id: id.clone(),
        state,
        pending_asks: ctx.router.pending_asks(id),
        exit_code,
    })
}

pub(crate) async fn list_agents(
    State(ctx): State<ApiContext>,
) -> Result<Json<AgentListResponse>, ApiError> {
    let mut agents = Vec::with_capacity(ctx.workflow.agents.len());
    for id in ctx.workflow.agents.keys() {
        agents.push(agent_info(&ctx, id)?);
    }
    Ok(Json(AgentListResponse { agents }))
}

pub(crate) async fn get_agent(
    State(ctx): State<ApiContext>,
    Path(id): Path<String>,
) -> Result<Json<AgentDetail>, ApiError> {
    let id = AgentId(id);
    let config = roster_agent(&ctx, &id)?.clone();
    let info = agent_info(&ctx, &id)?;
    let pty_cursor = ctx.pty.end_cursor(&id).map_err(ApiError::internal)?.0;
    Ok(Json(AgentDetail {
        info,
        dir: config.dir.display().to_string(),
        model: config.model,
        permission_mode: config.permission_mode,
        auto_clear: config.auto_clear,
        pty_cursor,
    }))
}

/// Reported state from the agent's own hooks (`SessionStart`/`Stop` → idle,
/// `UserPromptSubmit` → working). Only the agent itself may report: the identity in
/// `X-CoreTempo-Agent` must equal `{id}`, mirroring the reply handler's replier rule.
pub(crate) async fn report_state(
    State(ctx): State<ApiContext>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let req: ReportStateRequest = serde_json::from_slice(&body).map_err(|error| {
        ApiError::invalid(format!(
            "malformed state body: {error}; expected {{\"state\":\"working\"}} or \
             {{\"state\":\"idle\"}} — an agent reports only those two states"
        ))
    })?;
    let id = AgentId(id);
    roster_agent(&ctx, &id)?;
    let caller = auth::caller_origin(&ctx, &headers)?;
    if caller != Origin::Agent(id.clone()) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "wrong_agent",
            format!(
                "only agent '{}' may report its own state; you are '{caller}' — send \
                 X-CoreTempo-Agent: {} (the CORETEMPO_AGENT_ID of the reporting agent)",
                id.0, id.0
            ),
        ));
    }
    let state: AgentState = req.state.into();
    ctx.pty
        .report_state(&id, state)
        .map_err(ApiError::internal)?;
    Ok(Json(AgentStateResponse { agent: id, state }).into_response())
}

/// `POST /v1/agents/{id}/loop-done` (edge-semantics spec): the calling agent
/// declares its loop with `{id}` complete. Only agents own loops, so the
/// caller must identify via `X-CoreTempo-Agent`.
pub(crate) async fn loop_done(
    State(ctx): State<ApiContext>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let target = AgentId(id);
    roster_agent(&ctx, &target)?;
    let caller = auth::caller_origin(&ctx, &headers)?;
    let Origin::Agent(owner) = caller else {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "not_an_agent",
            format!(
                "only an agent may end a loop; you are '{caller}' — agents send \
                 X-CoreTempo-Agent: <their id> (set from CORETEMPO_AGENT_ID)"
            ),
        ));
    };
    ctx.router
        .mark_loop_done(&owner, &target)
        .map_err(|e| crate::api::map_router_error(e, &ctx.workflow))?;
    Ok(Json(serde_json::json!({
        "owner": owner,
        "target": target,
        "loop": "done",
    }))
    .into_response())
}

pub(crate) async fn restart_agent(
    State(ctx): State<ApiContext>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let id = AgentId(id);
    roster_agent(&ctx, &id)?;
    // Fail in-flight/queued messages to the agent and suppress stale reply injection
    // before the respawn kicks off (contract §4).
    ctx.router.on_agent_restarted(&id).await;
    ctx.pty.begin_restart(id.clone());
    let body = RestartResponse {
        agent: id,
        state: AgentState::Restarting,
    };
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}
