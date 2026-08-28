//! Files a session spawns with (spec 2026-08-27 §4): `settings.json` with the
//! turn-boundary hooks in *wait* mode and `Bash(tempo:*)` alone, and — for
//! `isolated_config` — a seeded `claude-config/`. They live under
//! `<sessions root>/<session-id>/` for the row's life: stop and resume keep
//! them (an isolated session's transcript is inside that config dir, and
//! `--resume` needs it), delete removes them.

use std::path::{Path, PathBuf};

use crate::api::auth::write_private_file;
use crate::claude_config::{ClaudeConfigError, write_config_dir};
use crate::pty::hooks::settings_json;
use crate::types::config::PermissionPrompt;
use crate::types::id::AgentId;

#[derive(Debug, thiserror::Error)]
pub enum SessionFilesError {
    #[error("cannot write session file '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    ClaudeConfig(#[from] ClaudeConfigError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFiles {
    pub dir: PathBuf,
    pub settings: PathBuf,
    pub config_dir: Option<PathBuf>,
}

pub struct SessionFileInputs<'a> {
    pub root: &'a Path,
    pub id: &'a AgentId,
    pub tempo_bin: &'a Path,
    pub isolated_config: bool,
}

#[must_use]
pub fn session_dir(root: &Path, id: &AgentId) -> PathBuf {
    root.join(&id.0)
}

/// The paths without writing them — what a boot-time roster rebuild needs.
#[must_use]
pub fn session_files(root: &Path, id: &AgentId, isolated_config: bool) -> SessionFiles {
    let dir = session_dir(root, id);
    SessionFiles {
        settings: dir.join("settings.json"),
        config_dir: isolated_config.then(|| dir.join("claude-config")),
        dir,
    }
}

/// # Errors
/// [`SessionFilesError`] naming the file that could not be written.
pub fn write_session_files(
    inputs: &SessionFileInputs<'_>,
) -> Result<SessionFiles, SessionFilesError> {
    let files = session_files(inputs.root, inputs.id, inputs.isolated_config);
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| SessionFilesError::Io { path, source }
    };
    std::fs::create_dir_all(&files.dir).map_err(io(&files.dir))?;
    write_private_file(
        &files.settings,
        &settings_json(inputs.tempo_bin, &[], &[], PermissionPrompt::Wait),
    )
    .map_err(io(&files.settings))?;
    if let Some(config_dir) = &files.config_dir {
        write_config_dir(config_dir, inputs.id, &[])?;
    }
    Ok(files)
}

/// Removes `<root>/<id>` and everything in it; already gone is fine.
///
/// # Errors
/// Any other filesystem error.
pub fn remove_session_files(root: &Path, id: &AgentId) -> std::io::Result<()> {
    match std::fs::remove_dir_all(session_dir(root, id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use crate::sessions::files::{
        SessionFileInputs, remove_session_files, session_files, write_session_files,
    };
    use crate::types::id::AgentId;

    #[test]
    fn a_plain_session_gets_a_wait_settings_file_and_nothing_else() {
        let t = tempfile::tempdir().expect("tmp");
        let id = AgentId("s-1".into());
        let files = write_session_files(&SessionFileInputs {
            root: t.path(),
            id: &id,
            tempo_bin: Path::new("/opt/bin/tempo"),
            isolated_config: false,
        })
        .expect("writes");
        assert_eq!(files.dir, t.path().join("s-1"));
        assert_eq!(files.settings, t.path().join("s-1").join("settings.json"));
        assert_eq!(files.config_dir, None);
        assert_eq!(
            files,
            session_files(t.path(), &id, false),
            "paths are derivable"
        );
        let mode = std::fs::metadata(&files.settings)
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&files.settings).expect("read"))
                .expect("json");
        assert_eq!(
            json["hooks"]["PermissionRequest"][0]["hooks"][0]["command"],
            "/opt/bin/tempo state blocked",
            "sessions wait for a human"
        );
        assert_eq!(
            json["permissions"]["allow"],
            serde_json::json!(["Bash(tempo:*)"])
        );
        let entries: Vec<String> = std::fs::read_dir(&files.dir)
            .expect("ls")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, ["settings.json"]);
    }

    #[test]
    fn an_isolated_session_gets_a_seeded_config_dir_that_survives_until_removed() {
        let t = tempfile::tempdir().expect("tmp");
        let id = AgentId("s-2".into());
        let files = write_session_files(&SessionFileInputs {
            root: t.path(),
            id: &id,
            tempo_bin: Path::new("/opt/bin/tempo"),
            isolated_config: true,
        })
        .expect("writes");
        let dir = files.config_dir.expect("config dir");
        assert_eq!(dir, t.path().join("s-2").join("claude-config"));
        assert_eq!(
            std::fs::read_to_string(dir.join(".claude.json")).expect("read"),
            crate::claude_config::CLAUDE_JSON
        );
        assert!(dir.join("settings.json").is_file());
        assert!(!dir.join("skills").exists());
        // Writing again (a resume never does, but a retry might) is idempotent.
        write_session_files(&SessionFileInputs {
            root: t.path(),
            id: &id,
            tempo_bin: Path::new("/opt/bin/tempo"),
            isolated_config: true,
        })
        .expect("rewrites");
        remove_session_files(t.path(), &id).expect("removes");
        assert!(!t.path().join("s-2").exists());
        remove_session_files(t.path(), &id).expect("a second remove is fine");
    }
}
