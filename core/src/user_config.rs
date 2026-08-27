//! `~/.coretempo/config.toml` — the user's global `CoreTempo` settings (spec
//! 2026-08-17 §1). Exactly one key today. Loaded once at process start by the
//! binary that embeds `core` (`coretempod`, the desktop shell); `core` itself
//! never reads it — the resolved [`crate::trust::TrustPolicy`] is passed in.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UserConfig {
    /// `true` lets `CoreTempo` grant Claude Code trust for every agent dir it
    /// spawns in, without a per-workflow opt-in.
    pub trust_agent_dirs: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum UserConfigError {
    #[error("cannot determine the home directory for ~/.coretempo/config.toml; set HOME")]
    NoHome,
    #[error("cannot read '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse '{path}': {message}; the only key is `trust_agent_dirs = true|false`")]
    Parse { path: PathBuf, message: String },
}

impl UserConfig {
    /// `$CORETEMPO_CONFIG` when set (tests, alternate profiles), else
    /// `~/.coretempo/config.toml`.
    ///
    /// # Errors
    /// [`UserConfigError::NoHome`] when neither the override nor a home dir exists.
    pub fn default_path() -> Result<PathBuf, UserConfigError> {
        UserConfig::default_path_from(std::env::var_os("CORETEMPO_CONFIG"), std::env::home_dir())
    }

    /// [`default_path`](UserConfig::default_path) with its inputs explicit.
    ///
    /// # Errors
    /// [`UserConfigError::NoHome`] when both are `None`.
    pub fn default_path_from(
        override_path: Option<std::ffi::OsString>,
        home: Option<PathBuf>,
    ) -> Result<PathBuf, UserConfigError> {
        if let Some(path) = override_path {
            return Ok(PathBuf::from(path));
        }
        home.map(|home| home.join(".coretempo").join("config.toml"))
            .ok_or(UserConfigError::NoHome)
    }

    /// Reads `path`; a missing file is the defaults.
    ///
    /// # Errors
    /// [`UserConfigError::Io`] for any read failure other than not-found,
    /// [`UserConfigError::Parse`] for bad TOML or an unknown key.
    pub fn load(path: &Path) -> Result<UserConfig, UserConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(UserConfig::default()),
            Err(source) => {
                return Err(UserConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        toml::from_str(&text).map_err(|e| UserConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// [`load`](UserConfig::load) at [`default_path`](UserConfig::default_path).
    ///
    /// # Errors
    /// As both.
    pub fn load_default() -> Result<UserConfig, UserConfigError> {
        UserConfig::load(&UserConfig::default_path()?)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::user_config::{UserConfig, UserConfigError};

    #[test]
    fn missing_file_is_the_defaults() {
        let cfg = UserConfig::load(Path::new("/nonexistent/coretempo/config.toml")).expect("ok");
        assert_eq!(cfg, UserConfig::default());
        assert!(!cfg.trust_agent_dirs);
    }

    #[test]
    fn the_one_key_parses() {
        let t = tempfile::tempdir().expect("tmpdir");
        let path = t.path().join("config.toml");
        std::fs::write(&path, "trust_agent_dirs = true\n").expect("write");
        assert!(UserConfig::load(&path).expect("parses").trust_agent_dirs);
        std::fs::write(&path, "").expect("write");
        assert!(
            !UserConfig::load(&path)
                .expect("empty parses")
                .trust_agent_dirs
        );
    }

    #[test]
    fn unknown_keys_and_bad_toml_are_parse_errors_naming_the_path() {
        let t = tempfile::tempdir().expect("tmpdir");
        let path = t.path().join("config.toml");
        std::fs::write(&path, "trust_agent_dirs = true\nother = 1\n").expect("write");
        let err = UserConfig::load(&path).expect_err("unknown key");
        let UserConfigError::Parse {
            path: reported,
            message,
        } = &err
        else {
            panic!("expected Parse, got {err:?}");
        };
        assert_eq!(*reported, path);
        assert!(message.contains("other"), "{message}");
        assert!(
            err.to_string().contains("trust_agent_dirs"),
            "names the only key: {err}"
        );
        std::fs::write(&path, "trust_agent_dirs = ").expect("write");
        assert!(matches!(
            UserConfig::load(&path),
            Err(UserConfigError::Parse { .. })
        ));
    }

    #[test]
    fn default_path_prefers_the_override_then_home() {
        let home = std::path::PathBuf::from("/home/op");
        assert_eq!(
            UserConfig::default_path_from(
                Some("/tmp/ct-test/config.toml".into()),
                Some(home.clone())
            )
            .expect("path"),
            Path::new("/tmp/ct-test/config.toml")
        );
        assert_eq!(
            UserConfig::default_path_from(None, Some(home)).expect("path"),
            Path::new("/home/op/.coretempo/config.toml")
        );
        assert!(matches!(
            UserConfig::default_path_from(None, None),
            Err(UserConfigError::NoHome)
        ));
    }
}
