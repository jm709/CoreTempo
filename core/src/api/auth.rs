//! Guard middleware (bearer token, Host validation, JSON content-type), request
//! attribution, and the `~/.coretempo/runs/<run_id>/api.json` writer.

use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::api::{ApiContext, ApiError};
use crate::types::message::Origin;
use crate::types::{AgentId, ApiFile, RunId, Token};

pub(crate) async fn guard(State(ctx): State<ApiContext>, req: Request, next: Next) -> Response {
    match check(&ctx, &req) {
        Ok(()) => next.run(req).await,
        Err(error) => error.into_response(),
    }
}

fn check(ctx: &ApiContext, req: &Request) -> Result<(), ApiError> {
    check_host(ctx, req)?;
    if req.uri().path() == "/v1/health" {
        return Ok(());
    }
    check_bearer(ctx, req)?;
    // A webhook caller posts whatever its sender emits, and the trigger body is
    // the kickoff message verbatim rather than a JSON document. The exemption is
    // the content type only: auth above still applies.
    if req.uri().path() != "/v1/trigger" {
        check_content_type(req)?;
    }
    Ok(())
}

fn check_host(ctx: &ApiContext, req: &Request) -> Result<(), ApiError> {
    let raw = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    require_host(raw, ctx.bind)
}

/// The `Host` guard as a check, for servers with no [`ApiContext`] (serve mode's
/// trigger listener). One message, one rule, both servers.
///
/// # Errors
/// 403 `invalid_host` when [`host_ok`] rejects `raw`.
pub fn require_host(raw: &str, bind: IpAddr) -> Result<(), ApiError> {
    if host_ok(raw, bind) {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "invalid_host",
        format!(
            "Host '{raw}' rejected; connect via 127.0.0.1, localhost, or [::1] — this \
             blocks DNS-rebinding and cross-origin browser requests"
        ),
    ))
}

/// Whether a `Host` header may reach a server bound to `bind`: loopback names and
/// the bind address itself, nothing else. Blocks DNS-rebinding and cross-origin
/// browser requests. Public because the trigger server authenticates its own
/// requests, without an [`ApiContext`].
#[must_use]
pub fn host_ok(raw: &str, bind: IpAddr) -> bool {
    let host = strip_port(raw);
    if host == "localhost" {
        return true;
    }
    let Ok(ip) = host.parse::<IpAddr>() else {
        return false;
    };
    ip.is_loopback() || ip == bind
}

/// Strips an optional `:port`, handling bracketed IPv6 (`[::1]:4820` → `::1`).
fn strip_port(raw: &str) -> &str {
    if let Some(rest) = raw.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    raw.split(':').next().unwrap_or(raw)
}

/// Constant-time `Authorization: Bearer <token>` check, usable without an
/// [`ApiContext`] (the trigger server has a token but no API context).
pub(crate) fn bearer_ok(token: &Token, headers: &HeaderMap) -> bool {
    let provided = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    provided.as_bytes().ct_eq(token.0.as_bytes()).into()
}

fn check_bearer(ctx: &ApiContext, req: &Request) -> Result<(), ApiError> {
    require_bearer(&ctx.token, req.headers())
}

/// The bearer guard as a check, for servers with no [`ApiContext`].
///
/// # Errors
/// 401 `unauthorized` when the `Authorization` header does not carry `token`.
pub fn require_bearer(token: &Token, headers: &HeaderMap) -> Result<(), ApiError> {
    if bearer_ok(token, headers) {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "missing or invalid bearer token; send 'Authorization: Bearer <token>' using the \
         token from CORETEMPO_TOKEN or ~/.coretempo/runs/current/api.json"
            .to_string(),
    ))
}

fn check_content_type(req: &Request) -> Result<(), ApiError> {
    if req.method() != Method::POST {
        return Ok(());
    }
    let ct = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ct
        .trim()
        .to_ascii_lowercase()
        .starts_with("application/json")
    {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
        format!(
            "Content-Type '{ct}' rejected; this API only speaks JSON — send \
             'Content-Type: application/json'"
        ),
    ))
}

fn valid_agent_id(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return false;
    }
    let head_ok = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    head_ok
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

fn request_id() -> String {
    use rand::RngExt;
    format!("{:08x}", rand::rng().random::<u32>())
}

/// Server-derived message origin (never trusted from the body): `X-CoreTempo-Agent` header
/// → `Origin::Agent` (must be a roster member), absent → `Origin::Http(<8-hex req id>)`.
pub(crate) fn caller_origin(ctx: &ApiContext, headers: &HeaderMap) -> Result<Origin, ApiError> {
    let Some(value) = headers.get("x-coretempo-agent") else {
        return Ok(Origin::Http(request_id()));
    };
    let raw = value.to_str().map_err(|_| {
        ApiError::invalid("X-CoreTempo-Agent header must be visible ASCII".to_string())
    })?;
    if !valid_agent_id(raw) {
        return Err(ApiError::invalid(format!(
            "X-CoreTempo-Agent '{raw}' is not a valid agent id \
             (pattern ^[a-z0-9][a-z0-9_-]{{0,31}}$)"
        )));
    }
    let id = AgentId(raw.to_string());
    if !ctx.workflow.agents.contains_key(&id) {
        return Err(ApiError::invalid(format!(
            "X-CoreTempo-Agent '{raw}' is not in the roster; roster: {}",
            crate::api::roster_list(&ctx.workflow)
        )));
    }
    Ok(Origin::Agent(id))
}

/// Default runs directory: `~/.coretempo/runs`.
#[must_use]
pub fn default_runs_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".coretempo/runs"))
}

/// Writes `contents` to `path`, truncating, with mode 0600 both on creation and on
/// an existing file (run-scoped files carry the API token or agent-controlling hooks).
///
/// # Errors
/// Propagates filesystem errors from opening, writing, or chmod'ing the file.
pub(crate) fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())?;
    std::fs::set_permissions(path, PermissionsExt::from_mode(0o600))
}

/// Writes `<runs_dir>/<run_id>/api.json` with mode 0600. Repointing the `current`
/// symlink is [`repoint_current`]: a serve-mode daemon runs workflows the user is
/// not attached to, and those must not steal `current` from an interactive run.
///
/// # Errors
/// Propagates filesystem errors from creating the run directory or writing `api.json`.
pub fn write_api_file(
    runs_dir: &Path,
    run_id: &RunId,
    port: u16,
    token: &Token,
) -> std::io::Result<PathBuf> {
    let dir = runs_dir.join(&run_id.0);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("api.json");
    let file = ApiFile {
        port,
        token: token.clone(),
        run_id: run_id.clone(),
    };
    let json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
    write_private_file(&path, &json)?;
    Ok(path)
}

/// Repoints the `<runs_dir>/current` symlink at `<run_id>`; this is what makes the
/// `tempo` CLI resolve to this run when it has no port in its environment.
///
/// # Errors
/// Propagates filesystem errors from removing or recreating the symlink.
pub fn repoint_current(runs_dir: &Path, run_id: &RunId) -> std::io::Result<()> {
    let current = runs_dir.join("current");
    if std::fs::symlink_metadata(&current).is_ok() {
        std::fs::remove_file(&current)?;
    }
    std::os::unix::fs::symlink(&run_id.0, &current)
}

#[cfg(test)]
mod tests {
    use crate::api::auth::{bearer_ok, host_ok, strip_port, valid_agent_id};
    use crate::types::Token;
    use axum::http::HeaderMap;
    use axum::http::header::AUTHORIZATION;
    use std::net::{IpAddr, Ipv4Addr};

    const LOOP: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    #[test]
    fn host_rules() {
        assert!(host_ok("127.0.0.1:4820", LOOP));
        assert!(host_ok("localhost", LOOP));
        assert!(host_ok("[::1]:4820", LOOP));
        assert!(!host_ok("evil.example.com", LOOP));
        assert!(!host_ok("192.168.1.5:4820", LOOP));
        let lan: IpAddr = "192.168.1.5".parse().expect("ip");
        assert!(host_ok("192.168.1.5:4820", lan));
        assert!(!host_ok("", LOOP));
    }

    #[test]
    fn bearer_rules() {
        let token = Token("ab".repeat(32));
        let headers = |value: &str| {
            let mut map = HeaderMap::new();
            map.insert(AUTHORIZATION, value.parse().expect("header value"));
            map
        };
        assert!(bearer_ok(&token, &headers(&format!("Bearer {}", token.0))));
        assert!(!bearer_ok(&token, &HeaderMap::new()));
        assert!(!bearer_ok(&token, &headers(&token.0)));
        assert!(!bearer_ok(&token, &headers("Bearer wrong")));
        // A prefix of the token is not the token.
        assert!(!bearer_ok(
            &token,
            &headers(&format!("Bearer {}", &token.0[..60]))
        ));
    }

    #[test]
    fn strip_port_handles_ipv6_brackets() {
        assert_eq!(strip_port("[::1]:4820"), "::1");
        assert_eq!(strip_port("127.0.0.1:4820"), "127.0.0.1");
        assert_eq!(strip_port("localhost"), "localhost");
    }

    #[test]
    fn agent_id_pattern() {
        assert!(valid_agent_id("builder"));
        assert!(valid_agent_id("a1-b_2"));
        assert!(!valid_agent_id(""));
        assert!(!valid_agent_id("Builder"));
        assert!(!valid_agent_id("-lead"));
        assert!(!valid_agent_id(&"x".repeat(33)));
    }
}
