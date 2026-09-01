//! An async `/v1` client for the sessions daemon.
//!
//! Every method fails as a [`CmdError`], so a route's answer crosses the Tauri
//! IPC boundary unchanged: the daemon's `code` and `message` are passed through
//! verbatim because the daemon's message is the one that names the fix. Only
//! failures the daemon never got to answer — an unreachable socket, a body that
//! is not the JSON the route promised — get a code of the shell's own.
//!
//! **Where the timeout lives.** Every request-shaped call goes through `send`,
//! which is the one place the deadline is applied. Streaming callers build their
//! request with `request` and never touch `send`, so an SSE stream that stays
//! open for hours is exempt by construction rather than by remembering to
//! override a client-wide default.

use std::time::Duration;

use coretempo_core::types::session::{
    CreateProjectRequest, CreateSessionRequest, DeleteSessionResponse, ProjectView, ResumeResponse,
    SessionView, SessionsHealth,
};
use reqwest::Method;
use serde::de::DeserializeOwned;

use crate::commands::CmdError;

/// How long a non-streaming route may take to answer in full. Generous because
/// the slow ones are genuinely slow — creating a session builds a worktree and
/// spawns Claude Code, behind the per-session mutex every lifecycle call holds.
/// This is not a latency budget; it is the bound that stops a wedged daemon from
/// leaving an IPC invoke pending forever with nothing for the UI to render.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One sessions daemon, addressed on loopback with its operator token.
#[derive(Clone)]
pub struct DaemonClient {
    base: String,
    token: String,
    timeout: Duration,
    http: reqwest::Client,
}

/// The API error envelope every non-2xx carries (contracts §5.2).
#[derive(serde::Deserialize)]
struct ApiErrorBody {
    error: ApiErrorInner,
}

#[derive(serde::Deserialize)]
struct ApiErrorInner {
    code: String,
    message: String,
}

impl DaemonClient {
    #[must_use]
    pub fn new(port: u16, token: String) -> DaemonClient {
        DaemonClient {
            base: format!("http://127.0.0.1:{port}"),
            token,
            timeout: REQUEST_TIMEOUT,
            http: reqwest::Client::new(),
        }
    }

    /// A shorter deadline than `REQUEST_TIMEOUT`, so a test need not wait one
    /// out. Same reason [`crate::sessions::discovery::Discovery::deadline`] is a
    /// field rather than a constant.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> DaemonClient {
        self.timeout = timeout;
        self
    }

    /// A request at `path` carrying the operator token, and **no deadline** —
    /// this is what the PTY and event streams build on, and they stay open for
    /// as long as the session lives. Anything that expects one whole response
    /// should go through `send` instead.
    pub(crate) fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
    }

    /// `GET /v1/health`, the liveness probe discovery connects on.
    ///
    /// # Errors
    /// The daemon's envelope, or `daemon_unreachable` when nothing answered.
    pub async fn health(&self) -> Result<SessionsHealth, CmdError> {
        self.json(self.request(Method::GET, "/v1/health")).await
    }

    /// # Errors
    /// The daemon's envelope, or `daemon_unreachable` when nothing answered.
    pub async fn list_sessions(&self) -> Result<Vec<SessionView>, CmdError> {
        self.json(self.request(Method::GET, "/v1/sessions")).await
    }

    /// # Errors
    /// The daemon's envelope — `untrusted`, `not_a_git_repo`, `dirty_worktree`
    /// and friends all arrive with the message that names the fix.
    pub async fn create_session(
        &self,
        req: &CreateSessionRequest,
    ) -> Result<SessionView, CmdError> {
        self.json(self.request(Method::POST, "/v1/sessions").json(req))
            .await
    }

    /// # Errors
    /// The daemon's envelope: `unknown_session`, or `wrong_state` when the
    /// session is not running.
    pub async fn stop_session(&self, id: &str) -> Result<SessionView, CmdError> {
        self.json(self.request(Method::POST, &format!("/v1/sessions/{id}/stop")))
            .await
    }

    /// # Errors
    /// The daemon's envelope: `unknown_session`, `wrong_state`, or
    /// `worktree_missing` when the worktree went away behind its back.
    pub async fn resume_session(&self, id: &str) -> Result<ResumeResponse, CmdError> {
        self.json(self.request(Method::POST, &format!("/v1/sessions/{id}/resume")))
            .await
    }

    /// # Errors
    /// The daemon's envelope: `unknown_session`, or `dirty_worktree` when the
    /// worktree has uncommitted work and `force` was not set.
    pub async fn delete_session(
        &self,
        id: &str,
        remove_worktree: bool,
        force: bool,
    ) -> Result<DeleteSessionResponse, CmdError> {
        let path = format!("/v1/sessions/{id}?remove_worktree={remove_worktree}&force={force}");
        self.json(self.request(Method::DELETE, &path)).await
    }

    /// # Errors
    /// The daemon's envelope, or `daemon_unreachable` when nothing answered.
    pub async fn list_projects(&self) -> Result<Vec<ProjectView>, CmdError> {
        self.json(self.request(Method::GET, "/v1/projects")).await
    }

    /// # Errors
    /// The daemon's envelope: `not_a_git_repo`, or `project_exists` when the
    /// root is already registered.
    pub async fn register_project(
        &self,
        req: &CreateProjectRequest,
    ) -> Result<ProjectView, CmdError> {
        self.json(self.request(Method::POST, "/v1/projects").json(req))
            .await
    }

    /// # Errors
    /// The daemon's envelope: `unknown_project`, or `project_in_use` when
    /// sessions still reference it.
    pub async fn forget_project(&self, id: &str) -> Result<(), CmdError> {
        self.no_content(self.request(Method::DELETE, &format!("/v1/projects/{id}")))
            .await
    }

    /// Types `data` into the session's PTY verbatim — the body is raw bytes,
    /// not JSON.
    ///
    /// # Errors
    /// The daemon's envelope: `unknown_agent`, or `agent_exited` when the
    /// session has no live process to write to.
    pub async fn write_pty(&self, id: &str, data: Vec<u8>) -> Result<(), CmdError> {
        let request = self
            .request(Method::POST, &format!("/v1/sessions/{id}/pty"))
            .body(data);
        self.no_content(request).await
    }

    /// # Errors
    /// The daemon's envelope: `unknown_agent` or `agent_exited`.
    pub async fn resize_pty(&self, id: &str, cols: u16, rows: u16) -> Result<(), CmdError> {
        let request = self
            .request(Method::POST, &format!("/v1/sessions/{id}/pty/resize"))
            .json(&serde_json::json!({ "cols": cols, "rows": rows }));
        self.no_content(request).await
    }

    /// # Errors
    /// The daemon's envelope: `unknown_agent` or `agent_exited`.
    pub async fn pause_pty(&self, id: &str, paused: bool) -> Result<(), CmdError> {
        let request = self
            .request(Method::POST, &format!("/v1/sessions/{id}/pty/pause"))
            .json(&serde_json::json!({ "paused": paused }));
        self.no_content(request).await
    }

    /// Sends `request` and decodes the JSON the route promised.
    async fn json<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, CmdError> {
        let response = self.send(request).await?;
        response.json::<T>().await.map_err(|err| {
            CmdError::new(
                "bad_response",
                format!("the sessions daemon sent malformed JSON: {err}"),
            )
        })
    }

    /// Sends `request` to a route that answers `204 No Content`; decoding the
    /// empty body as JSON would itself fail, so the status is the whole answer.
    async fn no_content(&self, request: reqwest::RequestBuilder) -> Result<(), CmdError> {
        self.send(request).await.map(|_| ())
    }

    /// Sends `request` under the client's deadline, turning a dead socket, a
    /// daemon that never answers, and a non-2xx all into a `CmdError`.
    ///
    /// The deadline is applied here and nowhere else, which is what makes it
    /// impossible for a decoded call to escape it: `json` and `no_content` are
    /// the only ways to read a response, and both come through here. Streams do
    /// not — see `request`.
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, CmdError> {
        let response = request
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|err| self.transport_err(&err))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        match response.json::<ApiErrorBody>().await {
            Ok(body) => Err(CmdError::new(&body.error.code, body.error.message)),
            Err(_) => Err(CmdError::new(
                "daemon_error",
                format!("the sessions daemon returned {status} with no error body"),
            )),
        }
    }

    /// A daemon that accepted the connection but went quiet needs a different
    /// fix from one that is not there at all, so the two do not share a code.
    fn transport_err(&self, err: &reqwest::Error) -> CmdError {
        if err.is_timeout() {
            return CmdError::new(
                "daemon_timeout",
                format!(
                    "the sessions daemon did not answer within {:?}; it may be wedged — check \
                     ~/.coretempo/sessions/daemon.log, then stop it and rerun 'coretempod sessions'",
                    self.timeout
                ),
            );
        }
        CmdError::new(
            "daemon_unreachable",
            format!(
                "could not reach the sessions daemon: {err}; it may have exited — reopen Sessions"
            ),
        )
    }
}
