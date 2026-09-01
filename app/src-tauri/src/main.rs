#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Context;
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

    fn build_test_app() -> anyhow::Result<tauri::App<tauri::test::MockRuntime>> {
        Ok(mock_builder()
            .manage(crate::state::AppState::default())
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

    fn invoke(
        webview: &tauri::WebviewWindow<tauri::test::MockRuntime>,
        cmd: &str,
        body: tauri::ipc::InvokeBody,
    ) -> anyhow::Result<serde_json::Value> {
        let response = tauri::test::get_ipc_response(
            webview,
            tauri::webview::InvokeRequest {
                cmd: cmd.into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: LOCAL_URL.parse()?,
                body,
                headers: tauri::http::HeaderMap::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .map_err(|err| anyhow::anyhow!("{cmd} ipc failed: {err}"))?;
        Ok(response.deserialize()?)
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
