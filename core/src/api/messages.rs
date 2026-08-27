//! `/v1/messages*` handlers: create (POST, `?wait` sugar), fetch (`?wait` long-poll),
//! traffic-log list. Contract §5.1 bodies; §6.1 long-poll semantics (300 s cap).

use std::collections::HashMap;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::api::{ApiContext, ApiError, auth, map_router_error};
use crate::router::MessageFilter;
use crate::types::message::{MessageKind, MessageStatus, Origin};
use crate::types::{AgentId, CreateMessageRequest, MessageId, MessageListResponse};

/// Clamp a `?wait=<secs>` value to the proxy-friendly 300 s cap (contract §6.1).
/// Public so serve mode's listener honours the same cap.
#[must_use]
pub fn wait_duration(secs: u64) -> Duration {
    Duration::from_secs(secs.min(300))
}

pub(crate) fn parse_body<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|error| {
        ApiError::invalid(format!(
            "malformed JSON body: {error}; expected e.g. \
             {{\"to\":\"builder\",\"kind\":\"ask\",\"body\":\"…\"}} for message creation or \
             {{\"code\":0,\"body\":\"…\"}} for replies"
        ))
    })
}

/// Reads a raw `?wait=<secs>` value. Public so serve mode parses it the same
/// way, with the same error message.
///
/// # Errors
/// 400 `invalid_request` when the value is not a whole number of seconds.
pub fn parse_wait(raw: Option<&str>) -> Result<Option<u64>, ApiError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    raw.parse::<u64>().map(Some).map_err(|_| {
        ApiError::invalid(format!(
            "wait='{raw}' is invalid; wait must be a whole number of seconds (max 300)"
        ))
    })
}

pub(crate) async fn create_message(
    State(ctx): State<ApiContext>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let req: CreateMessageRequest = parse_body(&body)?;
    let wait = parse_wait(params.get("wait").map(String::as_str))?;
    let from = auth::caller_origin(&ctx.core, &headers)?;
    let record = ctx
        .router
        .create_message(from, req.to, req.kind, req.body)
        .await
        .map_err(|e| map_router_error(e, &ctx.workflow))?;
    match wait {
        None => Ok((StatusCode::CREATED, Json(record)).into_response()),
        Some(secs) => {
            let record = ctx
                .router
                .wait_terminal(&record.id, wait_duration(secs))
                .await
                .map_err(|e| map_router_error(e, &ctx.workflow))?;
            Ok((StatusCode::OK, Json(record)).into_response())
        }
    }
}

pub(crate) async fn get_message(
    State(ctx): State<ApiContext>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let id = MessageId(id);
    let record = match parse_wait(params.get("wait").map(String::as_str))? {
        None => ctx.router.get_message(&id).await,
        Some(secs) => ctx.router.wait_terminal(&id, wait_duration(secs)).await,
    }
    .map_err(|e| map_router_error(e, &ctx.workflow))?;
    Ok(Json(record).into_response())
}

fn parse_enum<T: serde::de::DeserializeOwned>(
    raw: &str,
    what: &str,
    valid: &str,
) -> Result<T, ApiError> {
    serde_json::from_value(serde_json::Value::String(raw.to_string()))
        .map_err(|_| ApiError::invalid(format!("{what}='{raw}' is invalid; valid values: {valid}")))
}

fn parse_filter(params: &HashMap<String, String>) -> Result<MessageFilter, ApiError> {
    let status: Option<MessageStatus> = match params.get("status") {
        Some(raw) => Some(parse_enum(
            raw,
            "status",
            "queued, injected, working, replied, done, failed",
        )?),
        None => None,
    };
    let kind: Option<MessageKind> = match params.get("kind") {
        Some(raw) => Some(parse_enum(raw, "kind", "ask, send")?),
        None => None,
    };
    let from: Option<Origin> = match params.get("from") {
        Some(raw) => Some(raw.parse().map_err(|_| {
            ApiError::invalid(format!(
                "from='{raw}' is invalid; use 'agent:<id>', 'user', or 'http:<req-id>'"
            ))
        })?),
        None => None,
    };
    let limit = match params.get("limit") {
        Some(raw) => raw
            .parse::<u32>()
            .map_err(|_| {
                ApiError::invalid(format!(
                    "limit='{raw}' is invalid; limit must be an integer 1..=1000"
                ))
            })?
            .min(1000),
        None => 100,
    };
    Ok(MessageFilter {
        to: params.get("to").map(|s| AgentId(s.clone())),
        from,
        status,
        kind,
        since: params
            .get("since")
            .map(|s| crate::time::Timestamp(s.clone())),
        limit,
    })
}

pub(crate) async fn list_messages(
    State(ctx): State<ApiContext>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<MessageListResponse>, ApiError> {
    let filter = parse_filter(&params)?;
    let messages = ctx
        .router
        .list_messages(filter)
        .await
        .map_err(|e| map_router_error(e, &ctx.workflow))?;
    Ok(Json(MessageListResponse { messages }))
}

pub(crate) async fn reply_message(
    State(ctx): State<ApiContext>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let req: crate::types::ReplyRequest = parse_body(&body)?;
    if req.code > 1 {
        return Err(ApiError::invalid(format!(
            "reply code {} is invalid; code must be 0 (success) or 1 (failure)",
            req.code
        )));
    }
    let replier = auth::caller_origin(&ctx.core, &headers)?;
    let record = ctx
        .router
        .reply(replier, &MessageId(id), req.code, req.body)
        .await
        .map_err(|e| map_router_error(e, &ctx.workflow))?;
    Ok(Json(record).into_response())
}

#[cfg(test)]
mod tests {
    use crate::api::messages::wait_duration;
    use std::time::Duration;

    #[test]
    fn wait_is_capped_at_300s() {
        assert_eq!(wait_duration(30), Duration::from_secs(30));
        assert_eq!(wait_duration(300), Duration::from_mins(5));
        assert_eq!(wait_duration(9999), Duration::from_mins(5));
        assert_eq!(wait_duration(0), Duration::from_secs(0));
    }
}
