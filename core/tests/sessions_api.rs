//! The sessions daemon's `/v1` over a real listener (spec 2026-08-27 §6):
//! projects, the session lifecycle, hook-token scoping, the PTY routes and
//! the event stream, driven by a fake `claude` that reports its state through
//! the real hook route with its own token.
#![cfg(unix)]
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns are not covered by allow-*-in-tests"
)]

mod support;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use coretempo_core::api::sessions::{SessionsApi, build_sessions_router};
use coretempo_core::api::{ApiCore, PtyManagerSource, Roster, TokenAuth, serve_app};
use coretempo_core::sessions::SessionStore;
use coretempo_core::time::Timestamp;
use coretempo_core::types::AgentId;
use support::sessions::{Harness, OPERATOR, git, harness_http_with};

struct Api {
    h: Harness,
    port: u16,
    rt: Arc<tokio::runtime::Runtime>,
    project_id: std::cell::OnceCell<String>,
    _server: coretempo_core::api::ApiServerHandle,
}

fn core_bind() -> std::net::IpAddr {
    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
}

fn api(name: &str) -> Api {
    build_api(name, true)
}

fn api_untrusted(name: &str) -> Api {
    build_api(name, false)
}

fn build_api(name: &str, trusted: bool) -> Api {
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
    );
    let (h, server, port) = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // The fake agent reports through the real route, so the manager must
        // know the port before it spawns anything.
        let h = harness_http_with(name, port, trusted).await;
        let core = ApiCore {
            pty: Arc::new(PtyManagerSource(Arc::clone(h.mgr.pty()))),
            bus: h.bus.clone(),
            roster: Arc::clone(&h.mgr) as Arc<dyn Roster>,
            auth: Arc::clone(&h.mgr) as Arc<dyn TokenAuth>,
            token_provisioned: true,
            bind: core_bind(),
            port,
            started_at: Timestamp::now(),
            started: Instant::now(),
        };
        let app = build_sessions_router(SessionsApi {
            core,
            sessions: Arc::clone(&h.mgr),
        });
        let server = serve_app(listener, app, core_bind(), true).unwrap();
        (h, server, port)
    });
    Api {
        h,
        port,
        rt,
        project_id: std::cell::OnceCell::new(),
        _server: server,
    }
}

impl Drop for Api {
    /// Reaps the fake children; without it every test leaves a live bash
    /// behind when the runtime goes.
    fn drop(&mut self) {
        let mgr = Arc::clone(&self.h.mgr);
        self.rt.block_on(async move { mgr.shutdown().await });
    }
}

/// No socket timeout, deliberately: `timeout_global` makes ureq set
/// `SO_RCVTIMEO`, and a timed socket read returns `EINTR` when a signal lands
/// even under `SA_RESTART` — which every peer test's `git` child delivers as
/// `SIGCHLD` to this whole process. ureq does not retry it, so a budget here
/// buys spurious failures rather than safety. `support::open_sse` makes the
/// same choice; `Api::wait_state` keeps its own deadline.
fn http() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(None)
            .build(),
    )
}

impl Api {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
        token: &str,
    ) -> (u16, serde_json::Value) {
        let url = self.url(path);
        let auth = format!("Bearer {token}");
        let mut res = match (method, body) {
            ("GET", _) => http().get(&url).header("Authorization", auth).call(),
            ("DELETE", _) => http().delete(&url).header("Authorization", auth).call(),
            ("POST", Some(json)) => http()
                .post(&url)
                .header("Authorization", auth)
                .send_json(&json),
            ("POST", None) => http().post(&url).header("Authorization", auth).send_empty(),
            (other, _) => panic!("unsupported method {other}"),
        }
        .unwrap();
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
        )
    }

    fn get(&self, path: &str) -> (u16, serde_json::Value) {
        self.call("GET", path, None, OPERATOR)
    }
    fn post(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        self.call("POST", path, Some(body), OPERATOR)
    }
    fn delete(&self, path: &str) -> (u16, serde_json::Value) {
        self.call("DELETE", path, None, OPERATOR)
    }

    /// Registers the harness repo once; later calls reuse the id (a second
    /// POST would be 409 `project_exists`).
    fn project(&self) -> String {
        if let Some(id) = self.project_id.get() {
            return id.clone();
        }
        let (status, body) = self.post("/v1/projects", serde_json::json!({"path": self.h.repo}));
        assert_eq!(status, 201, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        let _ = self.project_id.set(id.clone());
        id
    }

    fn create(&self, extra: &serde_json::Value) -> serde_json::Value {
        let mut body = serde_json::json!({"project": self.project()});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, view) = self.post("/v1/sessions", body);
        assert_eq!(status, 201, "{view}");
        view
    }

    /// The session's own hook token, read out of the daemon's store.
    fn hook_token(&self, id: &str) -> String {
        let store = SessionStore::open(&self.h.root.join("sessions/sessions.db")).unwrap();
        let row = self
            .rt
            .block_on(store.get_session(&AgentId(id.to_string())))
            .unwrap();
        row.unwrap().hook_token.0
    }

    /// Opens an SSE stream and forwards each record's `event:` name down a
    /// channel, so a test can assert on what arrives *and* on what does not
    /// within a window. The reader thread ends when the server drops the
    /// connection at shutdown.
    fn watch_events(&self, path: &str) -> std::sync::mpsc::Receiver<String> {
        let mut res = http()
            .get(self.url(path))
            .header("Authorization", format!("Bearer {OPERATOR}"))
            .call()
            .unwrap();
        assert_eq!(res.status().as_u16(), 200);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(res.body_mut().as_reader()).lines() {
                let Ok(line) = line else { return };
                if let Some(event) = line.strip_prefix("event: ")
                    && tx.send(event.to_string()).is_err()
                {
                    return;
                }
            }
        });
        rx
    }

    /// Polls until `id` reads `want`, then returns a *fresh* view. A view's
    /// row is read before its live state, so the one that first matched can
    /// carry a row from just before the transition; everything a caller can
    /// conclude from the new state holds only of a read that starts after it.
    fn wait_state(&self, id: &str, want: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = serde_json::Value::Null;
        while Instant::now() < deadline {
            let (_, view) = self.get(&format!("/v1/sessions/{id}"));
            if view["state"] == want {
                return self.get(&format!("/v1/sessions/{id}")).1;
            }
            last = view;
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("session {id} never reached {want}; last: {last}");
    }
}

#[test]
fn health_projects_and_a_full_session_lifecycle_over_http() {
    let api = api("lifecycle");
    let (status, health) = api.get("/v1/health");
    assert_eq!(status, 200);
    assert_eq!(
        health,
        serde_json::json!({"ok": true, "sessions": {"live": 0, "total": 0}})
    );

    let view = api.create(&serde_json::json!({"worktree": true, "prompt": "hello there"}));
    let id = view["id"].as_str().unwrap().to_string();
    assert_eq!(view["title"], "hello there");
    assert_eq!(view["worktree_status"], "present");
    // The fake's SessionStart report lands through the hook route with its own token.
    let idle = api.wait_state(&id, "idle");
    assert_eq!(idle["claude_session_id"], "fake-sid");
    assert_eq!(api.get("/v1/health").1["sessions"]["live"], 1);
    let (status, list) = api.get("/v1/sessions");
    assert_eq!(status, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);

    let (status, body) = api.post(&format!("/v1/sessions/{id}/resume"), serde_json::json!({}));
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "wrong_state");

    let (status, stopped) = api.post(&format!("/v1/sessions/{id}/stop"), serde_json::json!({}));
    assert_eq!(status, 200, "{stopped}");
    assert_eq!(stopped["state"], "stopped");
    assert!(stopped["exit"].is_object(), "{stopped}");

    let (status, resumed) = api.post(&format!("/v1/sessions/{id}/resume"), serde_json::json!({}));
    assert_eq!(status, 200, "{resumed}");
    assert_eq!(resumed["resumed"], true);
    api.wait_state(&id, "idle");

    let (status, body) = api.delete(&format!("/v1/sessions/{id}"));
    assert_eq!(status, 409, "{body}");
    api.post(&format!("/v1/sessions/{id}/stop"), serde_json::json!({}));
    let (status, body) = api.delete(&format!("/v1/sessions/{id}?remove_worktree=true"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, serde_json::json!({"branch_kept": false}));
    let (status, body) = api.get(&format!("/v1/sessions/{id}"));
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "unknown_session");
}

#[test]
fn project_errors_carry_codes_and_fixes() {
    let api = api("projects");
    let project = api.project();
    let (status, body) = api.post("/v1/projects", serde_json::json!({"path": api.h.repo}));
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "project_exists");
    let (status, body) = api.post("/v1/projects", serde_json::json!({"path": api.h.root}));
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "not_a_git_repo");
    let (status, body) = api.post(
        "/v1/sessions",
        serde_json::json!({"project": project, "cwd": api.h.root}),
    );
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "cwd_outside_project");
    let (status, body) = api.post("/v1/sessions", serde_json::json!({"project": "p-nope"}));
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["code"], "unknown_project");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&project)
    );
    let (status, body) = api.post("/v1/sessions", serde_json::json!({"nonsense": true}));
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("\"project\""),
        "the malformed-body message names this route's shape: {body}"
    );
    let view = api.create(&serde_json::json!({}));
    let (status, body) = api.delete(&format!("/v1/projects/{project}"));
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "project_in_use");
    let id = view["id"].as_str().unwrap();
    api.post(&format!("/v1/sessions/{id}/stop"), serde_json::json!({}));
    api.delete(&format!("/v1/sessions/{id}"));
    let (status, _) = api.delete(&format!("/v1/projects/{project}"));
    assert_eq!(status, 204);
    let (status, list) = api.get("/v1/projects");
    assert_eq!(status, 200);
    assert!(list.as_array().unwrap().is_empty());
    let (status, body) = api.delete(&format!("/v1/projects/{project}"));
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["code"], "unknown_project");
}

#[test]
fn an_untrusted_root_is_409_untrusted_with_both_fixes() {
    let api = api_untrusted("untrusted");
    let project = api.project();
    let (status, body) = api.post("/v1/sessions", serde_json::json!({"project": project}));
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "untrusted");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("trust_agent_dirs = true")
    );
}

#[test]
fn a_dirty_worktree_is_422_with_the_summary_and_force_hint() {
    let api = api("dirty");
    let view = api.create(&serde_json::json!({"worktree": true}));
    let id = view["id"].as_str().unwrap().to_string();
    let wt = view["worktree"]["path"].as_str().unwrap().to_string();
    api.wait_state(&id, "idle");
    api.post(&format!("/v1/sessions/{id}/stop"), serde_json::json!({}));
    std::fs::write(Path::new(&wt).join("wip.txt"), "wip").unwrap();
    let (status, body) = api.delete(&format!("/v1/sessions/{id}?remove_worktree=true"));
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "dirty_worktree");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("wip.txt")
    );
    git(Path::new(&wt), &["add", "."]);
    git(Path::new(&wt), &["commit", "-q", "-m", "wip"]);
    let (status, body) = api.delete(&format!(
        "/v1/sessions/{id}?remove_worktree=true&force=true"
    ));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["branch_kept"], true);
}

#[test]
fn hook_tokens_reach_their_own_state_route_and_nothing_else() {
    let api = api("hook-tokens");
    let a = api.create(&serde_json::json!({}))["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = api.create(&serde_json::json!({}))["id"]
        .as_str()
        .unwrap()
        .to_string();
    api.wait_state(&a, "idle");
    api.wait_state(&b, "idle");
    let token_a = api.hook_token(&a);
    let (status, _) = api.call(
        "POST",
        &format!("/v1/agents/{a}/state"),
        Some(serde_json::json!({"state": "working"})),
        &token_a,
    );
    assert_eq!(status, 200);
    assert_eq!(api.get(&format!("/v1/sessions/{a}")).1["state"], "working");
    let (status, body) = api.call(
        "POST",
        &format!("/v1/agents/{b}/state"),
        Some(serde_json::json!({"state": "working"})),
        &token_a,
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "forbidden_scope");
    for path in ["/v1/sessions", "/v1/projects", "/v1/health"] {
        let (status, _) = api.call("GET", path, None, &token_a);
        assert_eq!(
            status,
            if path == "/v1/health" { 200 } else { 403 },
            "{path}"
        );
    }
    let (status, body) = api.call("POST", &format!("/v1/sessions/{a}/pty"), None, &token_a);
    assert_eq!(status, 403, "{body}");
    let (status, body) = api.call("GET", "/v1/sessions", None, "not-a-token");
    assert_eq!(status, 401, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sessions/api.json")
    );
}

/// Amendment 47's ordering: `report_state` stores the reported
/// `claude_session_id` *before* it publishes the state, so a caller that has
/// seen the new state can stop and resume without losing the conversation.
///
/// Racing the two writes would only be flaky, so the store's write lock is
/// held for the duration: with the store write first, no `agent.state` event
/// can reach the stream until the lock is released. The stream is the
/// observation point because it is the only one that does not itself read the
/// store — a blocked write holds the daemon's connection, so a `GET` would
/// simply queue behind it.
#[test]
fn a_reported_claude_session_id_is_stored_before_its_state_is_published() {
    let api = api("session-id");
    let id = api.create(&serde_json::json!({}))["id"]
        .as_str()
        .unwrap()
        .to_string();
    api.wait_state(&id, "idle");
    let url = api.url(&format!("/v1/agents/{id}/state"));
    let token = api.hook_token(&id);
    let events = api.watch_events(&format!("/v1/events?agent={id}&types=agent.state"));

    // `BEGIN IMMEDIATE` takes the write lock; the daemon's 5 s `busy_timeout`
    // makes its write wait for this rather than fail.
    let blocker =
        rusqlite::Connection::open(api.h.root.join("sessions/sessions.db")).expect("open db");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lock");

    let posted = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let res = http()
                .post(&url)
                .header("Authorization", format!("Bearer {token}"))
                .send_json(serde_json::json!({
                    "state": "working",
                    "claude_session_id": "held-sid",
                }))
                .expect("state report");
            assert_eq!(res.status().as_u16(), 200);
            posted.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        // Nothing about the report may be observable while the id cannot be
        // stored: no state event, and no answer to the reporting hook.
        let stray = events.recv_timeout(Duration::from_millis(500));
        assert!(
            stray.is_err(),
            "state event {stray:?} was published while its claude_session_id was unwritable"
        );
        assert!(
            !posted.load(std::sync::atomic::Ordering::SeqCst),
            "the report answered before its claude_session_id was stored"
        );
        blocker.execute_batch("COMMIT").expect("release the lock");
    });

    assert_eq!(
        events.recv_timeout(Duration::from_secs(10)).expect("event"),
        "agent.state"
    );
    let view = api.wait_state(&id, "working");
    assert_eq!(view["claude_session_id"], "held-sid");
    // And the stored id is what a resume passes to `claude`.
    api.post(&format!("/v1/sessions/{id}/stop"), serde_json::json!({}));
    let (status, resumed) = api.post(&format!("/v1/sessions/{id}/resume"), serde_json::json!({}));
    assert_eq!(status, 200, "{resumed}");
    assert_eq!(resumed["resumed"], true);
    assert_eq!(resumed["session"]["claude_session_id"], "held-sid");
}

#[test]
fn pty_write_resize_pause_and_the_stream_span_a_resume() {
    let api = api("pty");
    let id = api.create(&serde_json::json!({}))["id"]
        .as_str()
        .unwrap()
        .to_string();
    api.wait_state(&id, "idle");
    // Stream first, then write, then read what came back.
    let mut res = http()
        .get(api.url(&format!("/v1/sessions/{id}/pty")))
        .header("Authorization", format!("Bearer {OPERATOR}"))
        .call()
        .unwrap();
    let reader = BufReader::new(res.body_mut().as_reader());
    let (status, _) = api.call("POST", &format!("/v1/sessions/{id}/pty"), None, OPERATOR);
    assert_eq!(status, 204, "an empty write is fine");
    let r = http()
        .post(api.url(&format!("/v1/sessions/{id}/pty")))
        .header("Authorization", format!("Bearer {OPERATOR}"))
        .header("Content-Type", "application/octet-stream")
        .send(&b"ping\n"[..])
        .unwrap();
    assert_eq!(r.status().as_u16(), 204);
    let (status, _) = api.post(
        &format!("/v1/sessions/{id}/pty/resize"),
        serde_json::json!({"cols": 100, "rows": 30}),
    );
    assert_eq!(status, 204);
    let (status, _) = api.post(
        &format!("/v1/sessions/{id}/pty/pause"),
        serde_json::json!({"paused": false}),
    );
    assert_eq!(status, 204);
    let mut decoded = Vec::new();
    let mut last_id = 0u64;
    for line in reader.lines() {
        let line = line.unwrap();
        if let Some(id) = line.strip_prefix("id: ") {
            last_id = id.parse().unwrap();
        }
        if let Some(data) = line.strip_prefix("data: ") {
            let json: serde_json::Value = serde_json::from_str(data).unwrap();
            decoded.extend(support::b64_decode(json["b64"].as_str().unwrap()));
            if String::from_utf8_lossy(&decoded).contains("got:ping") {
                break;
            }
        }
    }
    assert!(last_id > 0);
    // After stop/resume the cursor keeps counting from where it was.
    api.post(&format!("/v1/sessions/{id}/stop"), serde_json::json!({}));
    let (status, body) = api.post(
        &format!("/v1/sessions/{id}/pty/resize"),
        serde_json::json!({"cols": 1, "rows": 1}),
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "agent_exited");
    api.post(&format!("/v1/sessions/{id}/resume"), serde_json::json!({}));
    let resumed = api.wait_state(&id, "idle");
    assert!(
        resumed["pty_cursor"].as_u64().unwrap() >= last_id,
        "{resumed}"
    );
}

#[test]
fn events_stream_the_session_lifecycle_in_order() {
    let api = api("events");
    let mut res = http()
        .get(api.url("/v1/events"))
        .header("Authorization", format!("Bearer {OPERATOR}"))
        .call()
        .unwrap();
    let reader = BufReader::new(res.body_mut().as_reader());
    let id = api.create(&serde_json::json!({}))["id"]
        .as_str()
        .unwrap()
        .to_string();
    api.wait_state(&id, "idle");
    api.post(&format!("/v1/sessions/{id}/stop"), serde_json::json!({}));
    api.post(&format!("/v1/sessions/{id}/resume"), serde_json::json!({}));
    api.wait_state(&id, "idle");
    api.post(&format!("/v1/sessions/{id}/stop"), serde_json::json!({}));
    api.delete(&format!("/v1/sessions/{id}"));
    let mut seen = Vec::new();
    for line in reader.lines() {
        let line = line.unwrap();
        if let Some(event) = line.strip_prefix("event: ") {
            if event.starts_with("session.") || event.starts_with("project.") {
                seen.push(event.to_string());
            }
            if event == "session.deleted" {
                break;
            }
        }
    }
    assert_eq!(
        seen,
        [
            "project.registered",
            "session.created",
            "session.stopped",
            "session.resumed",
            "session.stopped",
            "session.deleted"
        ]
    );
}

#[test]
fn an_unknown_route_names_every_route_the_daemon_answers() {
    let api = api("routes");
    let (status, body) = api.get("/v1/nope");
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request");
    let message = body["error"]["message"].as_str().unwrap();
    for route in [
        "GET /v1/health",
        "GET|POST /v1/sessions",
        "POST /v1/agents/{id}/state",
        "GET /v1/events",
    ] {
        assert!(message.contains(route), "{route} missing from: {message}");
    }
    // The run API's routes are not this daemon's.
    assert!(!message.contains("/v1/messages"), "{message}");
}
