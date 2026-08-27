//! Guard middleware (bearer token, Host validation, JSON content-type), request
//! attribution, and the `~/.coretempo/runs/<run_id>/api.json` writer.

use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HOST, TRANSFER_ENCODING};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::api::{ApiCore, ApiError, Caller};
use crate::types::message::Origin;
use crate::types::{AgentId, ApiFile, RunId, Token};

pub(crate) async fn guard(State(core): State<ApiCore>, req: Request, next: Next) -> Response {
    match check(&core, &req) {
        Ok(()) => next.run(req).await,
        Err(error) => error.into_response(),
    }
}

fn check(core: &ApiCore, req: &Request) -> Result<(), ApiError> {
    check_host(core, req)?;
    let path = req.uri().path();
    if path == "/v1/health" {
        return Ok(());
    }
    match core.auth.classify(bearer_of(req.headers())) {
        Caller::Operator => {}
        Caller::Hook(id) => require_hook_scope(&id, req.method(), path)?,
        Caller::Unknown => return Err(unauthorized(core.auth.hint())),
    }
    if !is_trigger_post(path) && !is_raw_pty_post(path) {
        check_content_type(req)?;
    }
    Ok(())
}

/// A hook token authorises exactly its own agent's state route (spec
/// 2026-08-27 §3): a tool a session's Claude runs can report state and
/// nothing else.
fn require_hook_scope(id: &AgentId, method: &Method, path: &str) -> Result<(), ApiError> {
    let own = format!("/v1/agents/{}/state", id.0);
    if method == Method::POST && path == own {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "forbidden_scope",
        format!(
            "this bearer token is agent '{}'s hook token; it authorises only POST {own} — \
             anything else needs the operator token",
            id.0
        ),
    ))
}

/// `POST …/pty` carries raw terminal bytes, not JSON.
fn is_raw_pty_post(path: &str) -> bool {
    path.ends_with("/pty")
}

/// The trigger routes. A webhook caller posts whatever its sender emits, and the
/// body is the kickoff message verbatim rather than a JSON document, so they are
/// exempt from the JSON content-type guard — the exemption is the content type
/// only: auth above still applies. The removed bare `/v1/trigger` keeps it so an
/// old caller reaches its 404 tombstone naming the new route, not a 415.
fn is_trigger_post(path: &str) -> bool {
    path == "/v1/trigger" || (path.starts_with("/v1/flows/") && path.ends_with("/trigger"))
}

fn check_host(core: &ApiCore, req: &Request) -> Result<(), ApiError> {
    let raw = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    require_host(raw, core.bind)
}

/// The `Host` guard as a check, for servers with no [`ApiCore`] (serve mode's
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
/// requests, without an [`ApiCore`].
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

/// The bearer value of `Authorization: Bearer <token>`, or `""`.
fn bearer_of(headers: &HeaderMap) -> &str {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
}

/// Constant-time token comparison; the run's `OperatorToken` and the sessions
/// daemon's token table both use it.
#[must_use]
pub fn token_matches(token: &Token, bearer: &str) -> bool {
    bearer.as_bytes().ct_eq(token.0.as_bytes()).into()
}

/// Constant-time `Authorization: Bearer <token>` check, usable without an
/// [`ApiCore`] (the trigger server has a token but no API context).
pub(crate) fn bearer_ok(token: &Token, headers: &HeaderMap) -> bool {
    token_matches(token, bearer_of(headers))
}

/// Where a 401'd caller can find the token this server wants (#57).
///
/// A run publishes its own in `~/.coretempo/runs/<run_id>/api.json`; a
/// `coretempod serve` daemon writes none and never repoints `current`, so the
/// only token it will ever accept is the one its environment gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenHint {
    /// A workflow run, whose `api.json` carries the token.
    Run,
    /// A headless `coretempod serve` daemon.
    Serve,
    /// The sessions daemon, whose token lives in its own `api.json`.
    Sessions,
}

impl TokenHint {
    fn advice(self) -> &'static str {
        match self {
            TokenHint::Run => {
                "using the token from CORETEMPO_TOKEN or ~/.coretempo/runs/current/api.json"
            }
            TokenHint::Serve => {
                "using the token this daemon was started with — `coretempod serve` is \
                 headless and writes no api.json, so its token is whatever \
                 CORETEMPO_TOKEN, --token-file/CORETEMPO_TOKEN_FILE, or [server] \
                 token_file provisioned"
            }
            TokenHint::Sessions => {
                "using the token in ~/.coretempo/sessions/api.json (written by \
                 `coretempod sessions` while it runs)"
            }
        }
    }
}

fn unauthorized(hint: TokenHint) -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        format!(
            "missing or invalid bearer token; send 'Authorization: Bearer <token>' {}",
            hint.advice()
        ),
    )
}

/// The bearer guard as a check, for servers with no [`ApiCore`]. `hint`
/// picks which token the refusal points the caller at.
///
/// # Errors
/// 401 `unauthorized` when the `Authorization` header does not carry `token`.
pub fn require_bearer(token: &Token, headers: &HeaderMap, hint: TokenHint) -> Result<(), ApiError> {
    if bearer_ok(token, headers) {
        return Ok(());
    }
    Err(unauthorized(hint))
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
    // A POST that carries nothing has nothing to misinterpret: `curl -X POST
    // .../restart` declares neither a body nor a type, and the body-less
    // endpoints (restart, loop-done) parse nothing anyway (#57). A declared
    // type still has to be JSON, and a body still has to declare one.
    if ct.is_empty() && no_body_declared(req.headers()) {
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

/// Whether the request says it carries no body: no `Transfer-Encoding`, and a
/// `Content-Length` that is absent or zero. Read off the headers rather than the
/// body, because the guard runs before any handler has consumed it — and an
/// unparseable `Content-Length` counts as a body, so a malformed request is
/// still refused rather than waved through.
fn no_body_declared(headers: &HeaderMap) -> bool {
    if headers.contains_key(TRANSFER_ENCODING) {
        return false;
    }
    match headers.get(CONTENT_LENGTH) {
        None => true,
        Some(len) => len
            .to_str()
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .is_some_and(|len| len == 0),
    }
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

/// Server-derived message origin (never trusted from the body). A hook token
/// decides identity by itself: `X-CoreTempo-Agent` may repeat it, never
/// change it. With the operator token the header names a roster member, and
/// no header means an anonymous HTTP caller.
pub(crate) fn caller_origin(core: &ApiCore, headers: &HeaderMap) -> Result<Origin, ApiError> {
    let claimed = claimed_agent(headers)?;
    if let Caller::Hook(id) = core.auth.classify(bearer_of(headers)) {
        return match claimed {
            Some(other) if other != id => Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "wrong_agent",
                format!(
                    "this hook token belongs to agent '{}' but X-CoreTempo-Agent says '{}'; \
                     drop the header or set it to '{}'",
                    id.0, other.0, id.0
                ),
            )),
            _ => Ok(Origin::Agent(id)),
        };
    }
    let Some(id) = claimed else {
        return Ok(Origin::Http(request_id()));
    };
    if !core.roster.contains(&id) {
        return Err(ApiError::invalid(format!(
            "X-CoreTempo-Agent '{}' is not in the roster; roster: {}",
            id.0,
            crate::api::roster_list(core.roster.as_ref())
        )));
    }
    Ok(Origin::Agent(id))
}

/// The validated `X-CoreTempo-Agent` header, if present.
fn claimed_agent(headers: &HeaderMap) -> Result<Option<AgentId>, ApiError> {
    let Some(value) = headers.get("x-coretempo-agent") else {
        return Ok(None);
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
    Ok(Some(AgentId(raw.to_string())))
}

/// Default runs directory: `~/.coretempo/runs`.
#[must_use]
pub fn default_runs_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".coretempo/runs"))
}

/// Writes `contents` to `path`, truncating, with mode 0600 both on creation and on
/// an existing file (run-scoped files carry the API token or agent-controlling hooks).
/// The contents are fsync'd before this returns, so a crash cannot leave a
/// zero-length file behind — callers that rename the result into place
/// (`trust::TrustStore::grant`) depend on that.
///
/// # Errors
/// Propagates filesystem errors from opening, writing, syncing, or chmod'ing the file.
pub(crate) fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
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
    fn body_declaration_rules() {
        use crate::api::auth::no_body_declared;
        use axum::http::header::{CONTENT_LENGTH, TRANSFER_ENCODING};

        let with = |name: axum::http::HeaderName, value: &str| {
            let mut map = HeaderMap::new();
            map.insert(name, value.parse().expect("header value"));
            map
        };
        assert!(no_body_declared(&HeaderMap::new()));
        assert!(no_body_declared(&with(CONTENT_LENGTH, "0")));
        assert!(!no_body_declared(&with(CONTENT_LENGTH, "12")));
        // Chunked bodies declare no length; they are still bodies.
        assert!(!no_body_declared(&with(TRANSFER_ENCODING, "chunked")));
        // A length nobody can parse is not a promise of an empty body.
        assert!(!no_body_declared(&with(CONTENT_LENGTH, "nonsense")));
    }

    #[test]
    fn a_hook_token_authorises_only_its_own_state_post() {
        use crate::api::auth::require_hook_scope;
        use crate::types::AgentId;
        use axum::http::Method;

        let id = AgentId("builder".to_string());
        assert!(
            require_hook_scope(&id, &Method::POST, "/v1/agents/builder/state").is_ok(),
            "its own state route must pass"
        );
        for (method, path) in [
            (Method::POST, "/v1/agents/planner/state"),
            (Method::GET, "/v1/agents/builder/state"),
            (Method::POST, "/v1/messages"),
        ] {
            let error = require_hook_scope(&id, &method, path).expect_err("must be refused");
            assert_eq!(error.code, "forbidden_scope", "{method} {path}");
            assert!(
                error.message.contains("POST /v1/agents/builder/state"),
                "{}",
                error.message
            );
        }
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
