//! Connection resolution: `CORETEMPO_PORT`/`CORETEMPO_TOKEN` env, else
//! `~/.coretempo/runs/current/api.json`. `tempo` never scans ports (contract §7.2).

use std::path::{Path, PathBuf};

use anyhow::Context;
use coretempo_core::types::ApiFile;

pub struct Connection {
    pub port: u16,
    pub token: String,
    pub agent_id: Option<String>,
}

pub fn resolve() -> anyhow::Result<Connection> {
    let api_file = default_api_file()?;
    resolve_with(|key| std::env::var(key).ok(), &api_file)
}

fn default_api_file() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .context("HOME is not set; cannot locate ~/.coretempo/runs/current/api.json")?;
    Ok(PathBuf::from(home).join(".coretempo/runs/current/api.json"))
}

pub(crate) fn resolve_with(
    env: impl Fn(&str) -> Option<String>,
    api_file: &Path,
) -> anyhow::Result<Connection> {
    let agent_id = env("CORETEMPO_AGENT_ID");
    if let (Some(port), Some(token)) = (env("CORETEMPO_PORT"), env("CORETEMPO_TOKEN")) {
        let port: u16 = port
            .parse()
            .with_context(|| format!("CORETEMPO_PORT '{port}' is not a valid port"))?;
        return Ok(Connection {
            port,
            token,
            agent_id,
        });
    }
    let text = std::fs::read_to_string(api_file).with_context(|| {
        format!(
            "no CORETEMPO_PORT/CORETEMPO_TOKEN in the environment and no run file at {}; \
         is a CoreTempo run active?",
            api_file.display()
        )
    })?;
    let file: ApiFile = serde_json::from_str(&text)
        .with_context(|| format!("malformed api.json at {}", api_file.display()))?;
    Ok(Connection {
        port: file.port,
        token: file.token.0,
        agent_id,
    })
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "tests assert inside Result-returning fns"
)]
mod tests {
    use crate::connect::resolve_with;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn temp_api_json(content: &str) -> anyhow::Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "tempo-connect-{}-{}",
            std::process::id(),
            content_hash(content)
        ));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("api.json");
        std::fs::write(&path, content)?;
        Ok(path)
    }

    fn content_hash(s: &str) -> usize {
        s.bytes().fold(s.len(), |acc, b| {
            acc.wrapping_mul(31).wrapping_add(b as usize)
        })
    }

    #[test]
    fn env_vars_win() -> anyhow::Result<()> {
        let conn = resolve_with(
            env(&[
                ("CORETEMPO_PORT", "5001"),
                ("CORETEMPO_TOKEN", "tok"),
                ("CORETEMPO_AGENT_ID", "planner"),
            ]),
            &PathBuf::from("/nonexistent/api.json"),
        )?;
        assert_eq!((conn.port, conn.token.as_str()), (5001, "tok"));
        assert_eq!(conn.agent_id.as_deref(), Some("planner"));
        Ok(())
    }

    #[test]
    fn falls_back_to_api_json() -> anyhow::Result<()> {
        let path = temp_api_json(r#"{"port":4820,"token":"deadbeef","run_id":"r-1f2e3d4c"}"#)?;
        let conn = resolve_with(env(&[]), &path)?;
        assert_eq!((conn.port, conn.token.as_str()), (4820, "deadbeef"));
        assert_eq!(conn.agent_id, None);
        Ok(())
    }

    #[test]
    fn missing_everything_errors_with_pointer_to_run() {
        let err = resolve_with(env(&[]), &PathBuf::from("/nonexistent/api.json"))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("CORETEMPO_PORT"), "err: {err}");
        assert!(err.contains("api.json"), "err: {err}");
    }

    #[test]
    fn bad_port_and_bad_json_error_clearly() -> anyhow::Result<()> {
        let err = resolve_with(
            env(&[("CORETEMPO_PORT", "not-a-port"), ("CORETEMPO_TOKEN", "t")]),
            &PathBuf::from("/nonexistent"),
        )
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
        assert!(err.contains("not-a-port"), "err: {err}");
        let path = temp_api_json("{ nope")?;
        let err = resolve_with(env(&[]), &path)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("malformed"), "err: {err}");
        Ok(())
    }
}
