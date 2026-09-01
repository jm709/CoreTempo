#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Context;
use coretempo_app_lib::sessions::commands as sessions;
use coretempo_app_lib::sessions::supervisor::SessionsState;
use coretempo_app_lib::{commands, state};

/// The frozen invoke surface (contracts §8.1). One factory so `main()` and the IPC tests can
/// never drift apart.
fn invoke_handler<R: tauri::Runtime>() -> impl Fn(tauri::ipc::Invoke<R>) -> bool {
    tauri::generate_handler![
        commands::snapshot,
        commands::run_start,
        commands::run_untrusted_dirs,
        commands::run_stop,
        commands::restart_agent,
        commands::subscribe_pty,
        commands::write_pty,
        commands::resize_pty,
        commands::pause_pty,
        commands::workflow_open,
        commands::workflow_save,
        commands::workflow_parse,
        commands::workflow_merge,
        commands::send_chat,
        commands::run_flows,
        commands::fire_flow,
        sessions::sessions_status,
        sessions::session_list,
        sessions::session_create,
        sessions::session_stop,
        sessions::session_resume,
        sessions::session_delete,
        sessions::project_list,
        sessions::project_register,
        sessions::project_forget,
        sessions::session_subscribe_pty,
        sessions::session_unsubscribe_pty,
        sessions::session_write_pty,
        sessions::session_resize_pty,
        sessions::session_pause_pty,
    ]
}

/// Fast-startup rule (spec §8): nothing blocking before the window shows. Only tracing init
/// and handler registration happen here; the core `Run` starts via the `run_start` command.
#[expect(
    clippy::exit,
    reason = "tauri::generate_context! expands to process::exit(101) if context creation panics"
)]
fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("CORETEMPO_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // The desktop must still show its window when ~/.coretempo/config.toml is
    // broken: an invisible failure from a launcher icon is worse than falling
    // back to asking. (`coretempod` hard-fails instead.)
    let trust_grant = match coretempo_core::user_config::UserConfig::load_default() {
        Ok(user) => user.trust_agent_dirs,
        Err(err) => {
            tracing::error!(%err, "ignoring ~/.coretempo/config.toml; trust will be asked for");
            false
        }
    };
    tauri::Builder::default()
        .manage(state::AppState::with_trust(trust_grant))
        .manage(SessionsState::default())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(invoke_handler())
        .build(tauri::generate_context!())
        .context("failed to build tauri app")?
        .run(|_app_handle, _event| {});
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "tests assert inside Result-returning fns"
)]
mod tests {
    use tauri::test::{INVOKE_KEY, mock_builder, mock_context, noop_assets};

    /// A sessions directory with no `api.json` and a binary that does not exist,
    /// so a test that kicks the supervisor can never reach — or start a daemon
    /// against — the operator's real `~/.coretempo/sessions`.
    fn scratch_discovery() -> coretempo_app_lib::sessions::discovery::Discovery {
        coretempo_app_lib::sessions::discovery::Discovery {
            sessions_dir: std::env::temp_dir()
                .join(format!("coretempo-shell-test-{}", std::process::id())),
            bin: std::path::PathBuf::from("/nonexistent-coretempod"),
            deadline: std::time::Duration::from_millis(200),
        }
    }

    fn build_test_app() -> anyhow::Result<tauri::App<tauri::test::MockRuntime>> {
        Ok(mock_builder()
            .manage(crate::state::AppState::default())
            .manage(crate::SessionsState::with_discovery(scratch_discovery()))
            .invoke_handler(crate::invoke_handler())
            .build(mock_context(noop_assets()))?)
    }

    /// The local (non-remote) origin: only local origins skip the ACL check for app commands.
    /// The custom-protocol URL is platform-shaped — `tauri://` everywhere but Windows.
    const LOCAL_URL: &str = if cfg!(windows) {
        "http://tauri.localhost"
    } else {
        "tauri://localhost"
    };

    fn request(
        cmd: &str,
        body: tauri::ipc::InvokeBody,
    ) -> anyhow::Result<tauri::webview::InvokeRequest> {
        Ok(tauri::webview::InvokeRequest {
            cmd: cmd.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: LOCAL_URL.parse()?,
            body,
            headers: tauri::http::HeaderMap::default(),
            invoke_key: INVOKE_KEY.to_string(),
        })
    }

    fn invoke(
        webview: &tauri::WebviewWindow<tauri::test::MockRuntime>,
        cmd: &str,
        body: tauri::ipc::InvokeBody,
    ) -> anyhow::Result<serde_json::Value> {
        let response = tauri::test::get_ipc_response(webview, request(cmd, body)?)
            .map_err(|err| anyhow::anyhow!("{cmd} ipc failed: {err}"))?;
        Ok(response.deserialize()?)
    }

    /// The `CmdError` a command that must fail answered with.
    fn invoke_err(
        webview: &tauri::WebviewWindow<tauri::test::MockRuntime>,
        cmd: &str,
        body: tauri::ipc::InvokeBody,
    ) -> anyhow::Result<serde_json::Value> {
        match tauri::test::get_ipc_response(webview, request(cmd, body)?) {
            Ok(_) => anyhow::bail!("{cmd} was expected to fail"),
            Err(err) => Ok(err),
        }
    }

    #[test]
    fn snapshot_is_registered_and_returns_empty_snapshot() -> anyhow::Result<()> {
        let app = build_test_app()?;
        let webview =
            tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default()).build()?;
        let value = invoke(&webview, "snapshot", tauri::ipc::InvokeBody::default())?;
        assert_eq!(value["run"], serde_json::Value::Null);
        assert_eq!(value["last_seq"], 0);
        Ok(())
    }

    /// `sessions_status` is what opening Sessions mode calls, and it is what
    /// kicks the supervisor off — so it must hunt for the daemon its *state*
    /// names, not the operator's real one. Nothing the scratch discovery above
    /// can find will ever answer, which is what makes `connected` a failure
    /// rather than a machine-dependent maybe.
    #[test]
    fn sessions_status_hunts_the_daemon_its_state_names() -> anyhow::Result<()> {
        let app = build_test_app()?;
        let webview =
            tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default()).build()?;
        let value = invoke(
            &webview,
            "sessions_status",
            tauri::ipc::InvokeBody::default(),
        )?;
        let state = value["state"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no state in {value}"))?;
        assert!(
            ["idle", "starting", "unreachable"].contains(&state),
            "reported {state}: the shell reached a daemon the test never provided"
        );
        Ok(())
    }

    /// Every daemon-backed command, with the argument names Task 7's webview
    /// invokes them by — a rename on either side is a break, and an unregistered
    /// command answers "not found" rather than a code, so this fails on both.
    ///
    /// With no connection they all answer the same code, which is the point: the
    /// UI has one thing to switch on.
    #[test]
    fn session_commands_are_registered_and_answer_unreachable() -> anyhow::Result<()> {
        let app = build_test_app()?;
        let webview =
            tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default()).build()?;
        let session = serde_json::json!({ "session": "s-1f2e3d4c" });
        let cases = [
            ("session_list", serde_json::json!({})),
            (
                "session_create",
                serde_json::json!({ "req": { "project": "p-0a1b2c3d" } }),
            ),
            ("session_stop", session.clone()),
            ("session_resume", session.clone()),
            (
                "session_delete",
                serde_json::json!({
                    "session": "s-1f2e3d4c", "remove_worktree": true, "force": false,
                }),
            ),
            ("project_list", serde_json::json!({})),
            (
                "project_register",
                serde_json::json!({ "path": "/home/u/proj", "name": "proj" }),
            ),
            (
                "project_forget",
                serde_json::json!({ "project": "p-0a1b2c3d" }),
            ),
            (
                "session_write_pty",
                serde_json::json!({ "session": "s-1f2e3d4c", "data": [104, 105, 13] }),
            ),
            (
                "session_resize_pty",
                serde_json::json!({ "session": "s-1f2e3d4c", "cols": 120, "rows": 40 }),
            ),
            (
                "session_pause_pty",
                serde_json::json!({ "session": "s-1f2e3d4c", "paused": true }),
            ),
            // `__CHANNEL__:<id>` is how the webview passes a Channel over IPC.
            (
                "session_subscribe_pty",
                serde_json::json!({
                    "session": "s-1f2e3d4c", "resume": false, "channel": "__CHANNEL__:1",
                }),
            ),
        ];
        for (cmd, args) in cases {
            let err = invoke_err(&webview, cmd, tauri::ipc::InvokeBody::Json(args))?;
            assert_eq!(err["code"], "daemon_unreachable", "{cmd} answered {err}");
        }
        Ok(())
    }

    /// Unsubscribing what was never subscribed is a no-op, not an error: the
    /// webview tears terminals down without tracking what is still attached.
    #[test]
    fn session_unsubscribe_pty_is_registered_and_always_succeeds() -> anyhow::Result<()> {
        let app = build_test_app()?;
        let webview =
            tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default()).build()?;
        let body = tauri::ipc::InvokeBody::Json(serde_json::json!({ "session": "s-1f2e3d4c" }));
        invoke(&webview, "session_unsubscribe_pty", body)?;
        Ok(())
    }

    #[test]
    fn workflow_parse_is_registered_with_snake_case_args() -> anyhow::Result<()> {
        let app = build_test_app()?;
        let webview =
            tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default()).build()?;
        let body =
            tauri::ipc::InvokeBody::Json(serde_json::json!({ "text": "this is not toml [" }));
        let value = invoke(&webview, "workflow_parse", body)?;
        assert_eq!(value["ok"], serde_json::Value::Bool(false));
        Ok(())
    }
}
