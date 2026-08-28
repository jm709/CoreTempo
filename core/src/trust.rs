//! Workspace trust (spec 2026-08-17 §1). Claude Code parks a fresh session on
//! its trust dialog for any git repository (or bare directory) it has not seen
//! before; `--dangerously-skip-permissions` does not skip it and no hook fires
//! while it waits. Trust is `hasTrustDialogAccepted = true` on
//! `projects["<root>"]` in `~/.claude.json`, keyed on the enclosing git
//! toplevel (observed live: a workdir inside a trusted repo needs no dialog).
//! `CoreTempo` checks that key for every agent's trust root before spawning
//! and — only when policy says so — writes it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::api::auth::write_private_file;
use crate::types::id::AgentId;

/// The path Claude Code keys trust on for `dir`: the toplevel of the git
/// repository containing it — walking up for `.git`, which is a directory in
/// a main checkout and a file in a linked worktree (the worktree dir is its
/// own toplevel) — else `dir` itself. Physical (canonicalized) when the path
/// exists, since Claude Code keys `projects` by `getcwd()`.
///
/// A missing relative path cannot be canonicalized, and the walk up such a
/// path reaches the empty path, whose `.git` would be resolved against the
/// process CWD — a root of `""` would be written into `~/.claude.json`. The
/// walk stops before that, so a missing relative dir is its own root.
#[must_use]
pub fn trust_root(dir: &Path) -> PathBuf {
    let physical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut cursor: Option<&Path> = Some(&physical);
    while let Some(candidate) = cursor {
        if candidate.join(".git").exists() {
            return candidate.to_path_buf();
        }
        cursor = candidate.parent().filter(|p| !p.as_os_str().is_empty());
    }
    physical
}

/// Claude Code's `~/.claude.json`, addressed explicitly so tests never touch
/// the real one. Reads treat a missing file as empty; `grant` creates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustStore {
    pub path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("cannot read or update Claude Code's trust store '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Claude Code's trust store '{path}' is not valid JSON: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "Claude Code's trust store '{path}' has an unexpected shape: the file's top level, \
         `projects`, or an entry under it, is not a JSON object; fix or move the file"
    )]
    Shape { path: PathBuf },
    #[error("{}", untrusted_message(.roots))]
    Untrusted { roots: Vec<PathBuf> },
}

fn untrusted_message(roots: &[PathBuf]) -> String {
    let list = roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Claude Code has not trusted these agent directories: {list}. Every agent spawned \
         there would park on the trust dialog and never reach idle. Either open `claude` once \
         in each directory and accept the dialog, or let CoreTempo grant trust: set \
         trust_agent_dirs = true under [server] in tempo.toml, or in ~/.coretempo/config.toml"
    )
}

/// May `CoreTempo` write trust for agent dirs? Either surface being `true`
/// means yes (spec §1); the embedding binary resolves it and hands it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrustPolicy {
    pub grant: bool,
}

impl TrustPolicy {
    #[must_use]
    pub fn resolve(user_config_grants: bool, workflow_grants: bool) -> TrustPolicy {
        TrustPolicy {
            grant: user_config_grants || workflow_grants,
        }
    }
}

const TRUST_KEY: &str = "hasTrustDialogAccepted";
static GRANT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl TrustStore {
    #[must_use]
    pub fn at(path: PathBuf) -> TrustStore {
        TrustStore { path }
    }

    /// The operator's `.claude.json` — `$CLAUDE_CONFIG_DIR/.claude.json` when
    /// that is set, else `~/.claude.json`; `None` when neither is known.
    #[must_use]
    pub fn from_env() -> Option<TrustStore> {
        crate::claude_config::operator_claude_json().map(TrustStore::at)
    }

    fn read(&self) -> Result<Value, TrustError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Value::Object(Map::new()));
            }
            Err(source) => {
                return Err(TrustError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        serde_json::from_slice(&bytes).map_err(|source| TrustError::Json {
            path: self.path.clone(),
            source,
        })
    }

    fn is_trusted(doc: &Value, root: &Path) -> bool {
        doc.get("projects")
            .and_then(|projects| projects.get(root.to_string_lossy().as_ref()))
            .and_then(|entry| entry.get(TRUST_KEY))
            .and_then(Value::as_bool)
            == Some(true)
    }

    /// The trust roots of `dirs` that lack `hasTrustDialogAccepted = true`,
    /// sorted and deduplicated. Empty means every dir may spawn without the
    /// dialog.
    ///
    /// # Errors
    /// [`TrustError::Io`] / [`TrustError::Json`] for a present but unreadable
    /// or malformed store.
    pub fn untrusted_roots<'a>(
        &self,
        dirs: impl IntoIterator<Item = &'a Path>,
    ) -> Result<Vec<PathBuf>, TrustError> {
        let doc = self.read()?;
        let roots: BTreeSet<PathBuf> = dirs.into_iter().map(trust_root).collect();
        Ok(roots
            .into_iter()
            .filter(|root| !Self::is_trusted(&doc, root))
            .collect())
    }

    /// Sets `hasTrustDialogAccepted = true` on `projects[<root>]` for every
    /// root, creating the file (just those entries) or the entries as needed
    /// and preserving everything else. Written 0600 to a uniquely named
    /// sibling temp file and renamed into place; if `path` is a symlink the
    /// rename replaces the link with a regular file (dotfile setups: keep the
    /// real file at `~/.claude.json`). Key order is not preserved (`serde_json`
    /// sorts); Claude Code does not care. Empty `roots` touches nothing.
    ///
    /// # Errors
    /// [`TrustError::Io`] / [`TrustError::Json`] / [`TrustError::Shape`].
    pub fn grant(&self, roots: &[PathBuf]) -> Result<(), TrustError> {
        if roots.is_empty() {
            return Ok(());
        }
        // One process may grant from several tasks at once (serve mode: up to
        // max_concurrent_runs preflights plus per-spawn gates). Serialize the
        // read→modify→rename so no two writers interleave on the user's real
        // file. Cross-process races (a live Claude session flushing) remain
        // and are what the per-spawn gate self-heals.
        let _serialized = GRANT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut doc = self.read()?;
        for root in roots {
            let entry = Self::project_entry(&mut doc, root, &self.path)?;
            entry.insert(TRUST_KEY.to_string(), Value::Bool(true));
        }
        self.write_atomic(&doc)
    }

    /// The listed keys of `projects[<root>]`, those present. Empty when the
    /// entry or every key is absent.
    ///
    /// # Errors
    /// [`TrustError::Io`] / [`TrustError::Json`].
    pub fn project_keys(
        &self,
        root: &Path,
        keys: &[&str],
    ) -> Result<Map<String, Value>, TrustError> {
        Ok(Self::keys_of(&self.read()?, root, keys))
    }

    /// Trusts `root` and sets `values` on `projects[<root>]` in one
    /// read-modify-rename, preserving everything else; the same atomic 0600
    /// write as [`TrustStore::grant`]. Returns whether it wrote — the file is
    /// left untouched when it already says exactly this.
    ///
    /// # Errors
    /// [`TrustError::Io`] / [`TrustError::Json`] / [`TrustError::Shape`].
    pub fn grant_with_keys(
        &self,
        root: &Path,
        values: &Map<String, Value>,
    ) -> Result<bool, TrustError> {
        let _serialized = GRANT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut doc = self.read()?;
        self.apply_project(&mut doc, root, values)
    }

    /// Trusts `to` and copies `keys` from `projects[<from>]` onto
    /// `projects[<to>]` — source and target read together, so the whole
    /// derivation is one read and at most one rename. Returns whether it
    /// wrote.
    ///
    /// This is what keeps a session spawn off the operator's live
    /// `~/.claude.json`: Claude Code rewrites that file on its own cadence
    /// (project history, costs, MCP approvals), and every read→rename of ours
    /// is a window in which one of its flushes is lost.
    ///
    /// # Errors
    /// [`TrustError::Io`] / [`TrustError::Json`] / [`TrustError::Shape`].
    pub fn derive_project(
        &self,
        from: &Path,
        to: &Path,
        keys: &[&str],
    ) -> Result<bool, TrustError> {
        let _serialized = GRANT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut doc = self.read()?;
        let values = Self::keys_of(&doc, from, keys);
        self.apply_project(&mut doc, to, &values)
    }

    /// The `keys` present on `projects[<root>]` of an already-read document.
    fn keys_of(doc: &Value, root: &Path, keys: &[&str]) -> Map<String, Value> {
        let mut out = Map::new();
        let Some(entry) = doc
            .get("projects")
            .and_then(|p| p.get(root.to_string_lossy().as_ref()))
        else {
            return out;
        };
        for key in keys {
            if let Some(value) = entry.get(*key) {
                out.insert((*key).to_string(), value.clone());
            }
        }
        out
    }

    /// Writes the trust key and `values` onto `projects[<root>]` of `doc`,
    /// renaming the result into place only when that changes something.
    fn apply_project(
        &self,
        doc: &mut Value,
        root: &Path,
        values: &Map<String, Value>,
    ) -> Result<bool, TrustError> {
        let entry = Self::project_entry(doc, root, &self.path)?;
        let mut changed = entry.get(TRUST_KEY).and_then(Value::as_bool) != Some(true);
        entry.insert(TRUST_KEY.to_string(), Value::Bool(true));
        for (key, value) in values {
            if entry.get(key) != Some(value) {
                changed = true;
                entry.insert(key.clone(), value.clone());
            }
        }
        if !changed {
            return Ok(false);
        }
        self.write_atomic(doc)?;
        Ok(true)
    }

    /// `projects[<root>]` as a mutable object, created on demand.
    fn project_entry<'a>(
        doc: &'a mut Value,
        root: &Path,
        path: &Path,
    ) -> Result<&'a mut Map<String, Value>, TrustError> {
        let shape = || TrustError::Shape {
            path: path.to_path_buf(),
        };
        let Value::Object(top) = doc else {
            return Err(shape());
        };
        let projects = top
            .entry("projects")
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(projects) = projects else {
            return Err(shape());
        };
        let entry = projects
            .entry(root.to_string_lossy().into_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(entry) = entry else {
            return Err(shape());
        };
        Ok(entry)
    }

    fn write_atomic(&self, doc: &Value) -> Result<(), TrustError> {
        let io = |source: std::io::Error| TrustError::Io {
            path: self.path.clone(),
            source,
        };
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).map_err(io)?;
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(
            ".claude.json.coretempo-{}-{seq}",
            std::process::id()
        ));
        let text = serde_json::to_string_pretty(doc).map_err(|source| TrustError::Json {
            path: self.path.clone(),
            source,
        })?;
        write_private_file(&tmp, &text).map_err(io)?;
        std::fs::rename(&tmp, &self.path).map_err(io)
    }
}

/// Spec §1 preflight over every agent dir about to spawn. With `grant`, the
/// untrusted roots are trusted (one `info` line each) and returned; without
/// it, [`TrustError::Untrusted`] names them all. Trusted roots cost nothing.
///
/// # Errors
/// [`TrustError::Untrusted`] when policy forbids granting; store errors otherwise.
pub fn preflight<'a>(
    store: &TrustStore,
    dirs: impl IntoIterator<Item = &'a Path>,
    policy: TrustPolicy,
) -> Result<Vec<PathBuf>, TrustError> {
    let untrusted = store.untrusted_roots(dirs)?;
    if untrusted.is_empty() {
        return Ok(untrusted);
    }
    if !policy.grant {
        return Err(TrustError::Untrusted { roots: untrusted });
    }
    store.grant(&untrusted)?;
    for root in &untrusted {
        tracing::info!(root = %root.display(), "granted Claude Code trust for an agent dir");
    }
    Ok(untrusted)
}

/// Re-applies the preflight to one agent immediately before each spawn
/// (initial and restart): a live Claude session flushes its in-memory
/// `~/.claude.json` on its own cadence and can revert a granted key minutes
/// later (observed on #1). For an `isolated_config` agent the same call then
/// mirrors the operator's decision into the managed dir's `.claude.json` —
/// the file that agent's Claude Code actually reads (spec 2026-08-24 §3).
/// The mirror is never a second consent source: it is written only after the
/// operator-store check passed. Installed on `PtyManager` by `Run::start_with`.
pub struct TrustGate {
    store: TrustStore,
    policy: TrustPolicy,
    mirrors: BTreeMap<AgentId, TrustStore>,
}

impl TrustGate {
    #[must_use]
    pub fn new(
        store: TrustStore,
        policy: TrustPolicy,
        mirrors: BTreeMap<AgentId, TrustStore>,
    ) -> TrustGate {
        TrustGate {
            store,
            policy,
            mirrors,
        }
    }
}

impl crate::pty::SpawnGate for TrustGate {
    fn before_spawn(&self, agent: &AgentId, dir: &Path) -> Result<(), String> {
        let granted = preflight(&self.store, [dir], self.policy).map_err(|e| e.to_string())?;
        if !granted.is_empty() {
            tracing::warn!(
                agent = %agent,
                "trust key was missing right before spawn (reverted by a live \
                 Claude session?); re-granted"
            );
        }
        if let Some(mirror) = self.mirrors.get(agent) {
            mirror
                .grant(&[trust_root(dir)])
                .map_err(|e| format!("cannot mirror trust into the managed config dir: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use crate::pty::SpawnGate;
    use crate::trust::{TrustError, TrustGate, TrustPolicy, TrustStore, preflight, trust_root};
    use crate::types::id::AgentId;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tmpdir")
    }

    fn physical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).expect("canonical")
    }

    #[test]
    fn trust_root_is_the_dir_itself_outside_any_repo() {
        let t = tmp();
        let dir = t.path().join("plain");
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(trust_root(&dir), physical(&dir));
    }

    #[test]
    fn trust_root_walks_up_to_the_git_toplevel() {
        let t = tmp();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect(".git dir");
        let nested = repo.join("crates").join("core");
        std::fs::create_dir_all(&nested).expect("nested");
        assert_eq!(trust_root(&nested), physical(&repo));
        assert_eq!(trust_root(&repo), physical(&repo));
    }

    #[test]
    fn trust_root_of_a_worktree_is_the_worktree_dir() {
        // A linked worktree's `.git` is a file pointing at the main repo's gitdir;
        // Claude Code keys trust on the worktree toplevel, not the main repo.
        let t = tmp();
        let main = t.path().join("main");
        std::fs::create_dir_all(main.join(".git")).expect("main .git");
        let wt = t.path().join("wt");
        std::fs::create_dir_all(wt.join("src")).expect("wt");
        std::fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").expect("file");
        assert_eq!(trust_root(&wt.join("src")), physical(&wt));
    }

    #[test]
    fn trust_root_resolves_symlinks_to_the_physical_path() {
        let t = tmp();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("repo");
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).expect("symlink");
        assert_eq!(trust_root(&link), physical(&repo));
    }

    #[test]
    fn trust_root_of_a_missing_dir_is_the_dir_as_given() {
        assert_eq!(
            trust_root(Path::new("/nonexistent/coretempo/agent")),
            PathBuf::from("/nonexistent/coretempo/agent")
        );
    }

    #[test]
    fn trust_root_of_a_missing_relative_dir_never_walks_to_the_empty_path() {
        // Walking up "projects/CoreTempo" reaches "", whose `.git` resolves against
        // the process CWD; a CWD inside a repo would otherwise yield a root of "".
        // Unit tests run with CWD = core/, which has no `.git`, so the CWD is moved
        // to a temp dir that does. It is restored before asserting so a failure
        // cannot leak the change into another test.
        let t = tmp();
        std::fs::create_dir_all(t.path().join(".git")).expect(".git");
        let original = std::env::current_dir().expect("cwd");
        let relative = Path::new("projects/CoreTempo");
        std::env::set_current_dir(t.path()).expect("chdir");
        let got = trust_root(relative);
        std::env::set_current_dir(&original).expect("restore cwd");
        assert_eq!(got, PathBuf::from("projects/CoreTempo"));
    }

    /// A store whose file already trusts `trusted` and carries unrelated content.
    fn store_with(t: &tempfile::TempDir, trusted: &Path) -> TrustStore {
        let path = t.path().join(".claude.json");
        let doc = json!({
            "numStartups": 12,
            "mcpServers": {"mailbox": {"command": "mailbox-mcp"}},
            "projects": {
                trusted.to_string_lossy(): {
                    "allowedTools": ["Bash(tempo:*)"],
                    "hasTrustDialogAccepted": true
                },
                "/home/u/other": {"hasTrustDialogAccepted": false}
            }
        });
        std::fs::write(&path, doc.to_string()).expect("write");
        TrustStore::at(path)
    }

    #[test]
    fn untrusted_roots_skips_trusted_roots_and_their_subdirs_and_dedupes() {
        let t = tmp();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("repo");
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).expect("sub");
        let fresh = t.path().join("fresh");
        std::fs::create_dir_all(fresh.join("a")).expect("fresh");
        std::fs::create_dir_all(fresh.join("b")).expect("fresh");
        let store = store_with(&t, &physical(&repo));
        let (fresh_a, fresh_b) = (fresh.join("a"), fresh.join("b"));
        let dirs = [
            repo.as_path(),
            sub.as_path(),
            fresh_a.as_path(),
            fresh_b.as_path(),
        ];
        let got = store.untrusted_roots(dirs).expect("reads");
        assert_eq!(
            got,
            vec![physical(&fresh.join("a")), physical(&fresh.join("b"))],
            "the trusted repo and its subdir are skipped; fresh dirs are their own roots"
        );
    }

    #[test]
    fn a_false_flag_counts_as_untrusted() {
        let t = tmp();
        let store = store_with(&t, Path::new("/home/u/trusted"));
        let got = store
            .untrusted_roots([Path::new("/home/u/other")])
            .expect("reads");
        assert_eq!(got, vec![PathBuf::from("/home/u/other")]);
    }

    #[test]
    fn missing_store_treats_everything_as_untrusted() {
        let t = tmp();
        let store = TrustStore::at(t.path().join("absent.json"));
        let got = store
            .untrusted_roots([Path::new("/x")])
            .expect("missing file is fine");
        assert_eq!(got, vec![PathBuf::from("/x")]);
    }

    #[test]
    fn grant_creates_a_private_file_with_just_the_project_entries() {
        let t = tmp();
        let path = t.path().join(".claude.json");
        let store = TrustStore::at(path.clone());
        store
            .grant(&[PathBuf::from("/w/one"), PathBuf::from("/w/two")])
            .expect("grants");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(
            doc,
            json!({"projects": {
                "/w/one": {"hasTrustDialogAccepted": true},
                "/w/two": {"hasTrustDialogAccepted": true}
            }})
        );
        assert!(
            store
                .untrusted_roots([Path::new("/w/one")])
                .expect("reads")
                .is_empty()
        );
    }

    #[test]
    fn grant_preserves_unrelated_content_and_sibling_keys() {
        let t = tmp();
        let store = store_with(&t, Path::new("/home/u/trusted"));
        store
            .grant(&[PathBuf::from("/home/u/other")])
            .expect("grants");
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&store.path).expect("read"))
                .expect("json");
        assert_eq!(doc["numStartups"], 12);
        assert_eq!(doc["mcpServers"]["mailbox"]["command"], "mailbox-mcp");
        assert_eq!(
            doc["projects"]["/home/u/trusted"]["allowedTools"][0],
            "Bash(tempo:*)"
        );
        assert_eq!(
            doc["projects"]["/home/u/trusted"]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(
            doc["projects"]["/home/u/other"]["hasTrustDialogAccepted"],
            true
        );
        let mode = std::fs::metadata(&store.path)
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert!(
            std::fs::read_dir(t.path()).expect("dir").all(|e| !e
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains("coretempo")),
            "no temp file left behind"
        );
    }

    #[test]
    fn grant_of_nothing_never_touches_the_file() {
        let t = tmp();
        let path = t.path().join(".claude.json");
        std::fs::write(&path, "not json").expect("write");
        TrustStore::at(path.clone()).grant(&[]).expect("no-op");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "not json");
    }

    #[test]
    fn a_malformed_store_is_a_typed_error() {
        let t = tmp();
        let path = t.path().join(".claude.json");
        std::fs::write(&path, "{ nope").expect("write");
        let err = TrustStore::at(path.clone())
            .untrusted_roots([Path::new("/x")])
            .expect_err("malformed");
        let TrustError::Json { path: reported, .. } = &err else {
            panic!("expected Json, got {err:?}");
        };
        assert_eq!(*reported, path);
    }

    #[test]
    fn a_non_object_projects_value_is_a_shape_error_on_grant() {
        let t = tmp();
        let path = t.path().join(".claude.json");
        std::fs::write(&path, r#"{"projects": []}"#).expect("write");
        let err = TrustStore::at(path.clone())
            .grant(&[PathBuf::from("/x")])
            .expect_err("bad shape");
        let TrustError::Shape { path: reported } = &err else {
            panic!("expected Shape, got {err:?}");
        };
        assert_eq!(*reported, path);
    }

    #[test]
    fn a_directory_at_the_store_path_is_an_io_error() {
        let t = tmp();
        let path = t.path().join("claude-dir");
        std::fs::create_dir_all(&path).expect("dir");
        let err = TrustStore::at(path.clone())
            .untrusted_roots([Path::new("/x")])
            .expect_err("EISDIR");
        let TrustError::Io { path: reported, .. } = &err else {
            panic!("expected Io, got {err:?}");
        };
        assert_eq!(*reported, path);
    }

    #[test]
    fn a_non_object_project_entry_is_a_shape_error_on_grant() {
        let t = tmp();
        let path = t.path().join(".claude.json");
        std::fs::write(&path, r#"{"projects": {"/x": 7}}"#).expect("write");
        let err = TrustStore::at(path.clone())
            .grant(&[PathBuf::from("/x")])
            .expect_err("bad entry");
        assert!(matches!(err, TrustError::Shape { .. }), "{err:?}");
        std::fs::write(&path, "[]").expect("write");
        let err = TrustStore::at(path)
            .grant(&[PathBuf::from("/x")])
            .expect_err("bad top");
        assert!(matches!(err, TrustError::Shape { .. }), "{err:?}");
    }

    #[test]
    fn preflight_matrix() {
        let t = tmp();
        let trusted = t.path().join("trusted");
        std::fs::create_dir_all(&trusted).expect("dir");
        let fresh = t.path().join("fresh");
        std::fs::create_dir_all(&fresh).expect("dir");
        let store = store_with(&t, &physical(&trusted));
        let grant = TrustPolicy { grant: true };
        let no_grant = TrustPolicy { grant: false };

        // trusted × any policy → nothing to do, nothing granted
        assert!(
            preflight(&store, [trusted.as_path()], no_grant)
                .expect("ok")
                .is_empty()
        );
        assert!(
            preflight(&store, [trusted.as_path()], grant)
                .expect("ok")
                .is_empty()
        );
        // untrusted × no grant → Untrusted naming the root
        let err = preflight(&store, [fresh.as_path()], no_grant).expect_err("refuses");
        let TrustError::Untrusted { roots } = &err else {
            panic!("expected Untrusted, got {err:?}");
        };
        assert_eq!(*roots, vec![physical(&fresh)]);
        // untrusted × grant → granted and reported
        let granted = preflight(&store, [fresh.as_path()], grant).expect("grants");
        assert_eq!(granted, vec![physical(&fresh)]);
        assert!(
            preflight(&store, [fresh.as_path()], no_grant)
                .expect("now trusted")
                .is_empty()
        );
    }

    #[test]
    fn untrusted_message_names_every_root_and_both_fixes() {
        let err = TrustError::Untrusted {
            roots: vec![PathBuf::from("/w/one"), PathBuf::from("/w/two")],
        };
        let text = err.to_string();
        for expected in [
            "/w/one",
            "/w/two",
            "trust dialog",
            "open `claude`",
            "trust_agent_dirs = true",
            "[server]",
            "~/.coretempo/config.toml",
        ] {
            assert!(text.contains(expected), "{expected:?} missing from: {text}");
        }
    }

    #[test]
    fn policy_resolves_as_or() {
        assert!(!TrustPolicy::resolve(false, false).grant);
        assert!(TrustPolicy::resolve(true, false).grant);
        assert!(TrustPolicy::resolve(false, true).grant);
        assert_eq!(TrustPolicy::default(), TrustPolicy { grant: false });
    }

    #[test]
    fn from_env_points_at_a_claude_json() {
        // HOME is not mutated here (other tests in this binary read it); only the
        // shape of the derived path is checked.
        let store = TrustStore::from_env().expect("HOME is set in CI and dev");
        assert!(
            store.path.ends_with(".claude.json"),
            "{}",
            store.path.display()
        );
    }

    fn trusted_operator_store(t: &tempfile::TempDir, root: &Path) -> TrustStore {
        let store = TrustStore::at(t.path().join("operator.claude.json"));
        store.grant(&[root.to_path_buf()]).expect("grant");
        store
    }

    #[test]
    fn gate_mirrors_an_operator_trusted_root_into_the_agent_store() {
        let t = tmp();
        let dir = t.path().join("agent");
        std::fs::create_dir_all(&dir).expect("dir");
        let root = trust_root(&dir);
        let operator = trusted_operator_store(&t, &root);
        let managed = t.path().join("claude-config-iso").join(".claude.json");
        std::fs::create_dir_all(managed.parent().expect("parent")).expect("mk");
        std::fs::write(
            &managed,
            r#"{"hasCompletedOnboarding":true,"theme":"dark"}"#,
        )
        .expect("seed");
        let mirror = TrustStore::at(managed.clone());
        let id = AgentId("iso".into());
        let gate = TrustGate::new(
            operator,
            TrustPolicy { grant: false },
            BTreeMap::from([(id.clone(), TrustStore::at(managed.clone()))]),
        );

        gate.before_spawn(&id, &dir).expect("spawn allowed");

        assert!(
            mirror
                .untrusted_roots([dir.as_path()])
                .expect("read")
                .is_empty(),
            "mirror carries the key"
        );
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&managed).expect("read")).expect("json");
        assert_eq!(
            doc["hasCompletedOnboarding"], true,
            "Claude Code's own keys are kept"
        );
        assert_eq!(doc["theme"], "dark");
    }

    #[test]
    fn gate_refuses_before_touching_the_mirror_when_the_operator_never_trusted() {
        let t = tmp();
        let dir = t.path().join("agent");
        std::fs::create_dir_all(&dir).expect("dir");
        let operator = TrustStore::at(t.path().join("operator.claude.json"));
        let managed = t.path().join("claude-config-iso").join(".claude.json");
        std::fs::create_dir_all(managed.parent().expect("parent")).expect("mk");
        std::fs::write(&managed, r#"{"hasCompletedOnboarding":true}"#).expect("seed");
        let id = AgentId("iso".into());
        let gate = TrustGate::new(
            operator,
            TrustPolicy { grant: false },
            BTreeMap::from([(id.clone(), TrustStore::at(managed.clone()))]),
        );

        let reason = gate.before_spawn(&id, &dir).expect_err("refused");
        assert!(reason.contains("trust_agent_dirs = true"), "{reason}");
        assert_eq!(
            std::fs::read_to_string(&managed).expect("read"),
            r#"{"hasCompletedOnboarding":true}"#,
            "mirror untouched"
        );
    }

    #[test]
    fn gate_mirrors_are_keyed_by_agent_id() {
        let t = tmp();
        let dir = t.path().join("agent");
        std::fs::create_dir_all(&dir).expect("dir");
        let operator = trusted_operator_store(&t, &trust_root(&dir));
        let managed = t.path().join("claude-config-iso").join(".claude.json");
        std::fs::create_dir_all(managed.parent().expect("parent")).expect("mk");
        let seed = r#"{"hasCompletedOnboarding":true}"#;
        std::fs::write(&managed, seed).expect("seed");
        let gate = TrustGate::new(
            operator,
            TrustPolicy { grant: false },
            BTreeMap::from([(AgentId("iso".into()), TrustStore::at(managed.clone()))]),
        );

        gate.before_spawn(&AgentId("plain".into()), &dir)
            .expect("spawn allowed");

        assert_eq!(
            std::fs::read_to_string(&managed).expect("read"),
            seed,
            "another agent's spawn never writes iso's mirror"
        );
    }

    #[test]
    fn gate_without_a_mirror_for_the_agent_only_checks_the_operator_store() {
        let t = tmp();
        let dir = t.path().join("agent");
        std::fs::create_dir_all(&dir).expect("dir");
        let operator = trusted_operator_store(&t, &trust_root(&dir));
        let gate = TrustGate::new(operator, TrustPolicy { grant: false }, BTreeMap::new());
        gate.before_spawn(&AgentId("plain".into()), &dir)
            .expect("spawn allowed");
    }

    #[test]
    fn project_keys_copy_between_roots_and_skip_absent_ones() {
        let t = tmp();
        let store = store_with(&t, Path::new("/home/u/trusted"));
        assert!(
            store
                .grant_with_keys(
                    Path::new("/home/u/trusted"),
                    json!({
                        "enabledMcpjsonServers": ["mailbox"],
                        "enableAllProjectMcpServers": false
                    })
                    .as_object()
                    .expect("object"),
                )
                .expect("writes")
        );
        let keys = store
            .project_keys(
                Path::new("/home/u/trusted"),
                &[
                    "enabledMcpjsonServers",
                    "disabledMcpjsonServers",
                    "enableAllProjectMcpServers",
                ],
            )
            .expect("reads");
        assert_eq!(keys.len(), 2, "absent keys are skipped: {keys:?}");
        assert_eq!(keys["enabledMcpjsonServers"], json!(["mailbox"]));
        assert!(
            store
                .derive_project(
                    Path::new("/home/u/trusted"),
                    Path::new("/w/wt"),
                    &[
                        "enabledMcpjsonServers",
                        "disabledMcpjsonServers",
                        "enableAllProjectMcpServers",
                    ],
                )
                .expect("derives into a new entry")
        );
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&store.path).expect("read"))
                .expect("json");
        assert_eq!(
            doc["projects"]["/w/wt"]["enabledMcpjsonServers"],
            json!(["mailbox"])
        );
        assert_eq!(doc["projects"]["/w/wt"]["hasTrustDialogAccepted"], true);
        assert_eq!(
            doc["projects"]["/home/u/trusted"]["allowedTools"][0],
            "Bash(tempo:*)"
        );
        assert!(
            store
                .project_keys(Path::new("/nowhere"), &["x"])
                .expect("reads")
                .is_empty()
        );
    }

    /// The write is what races a live Claude Code flush, so an unchanged
    /// entry must not produce one.
    #[test]
    fn granting_what_is_already_there_writes_nothing() {
        let t = tmp();
        let store = store_with(&t, Path::new("/home/u/trusted"));
        let values = json!({"enabledMcpjsonServers": ["mailbox"]})
            .as_object()
            .cloned()
            .expect("object");
        assert!(
            store
                .grant_with_keys(Path::new("/home/u/trusted"), &values)
                .expect("first write")
        );
        assert!(
            !store
                .grant_with_keys(Path::new("/home/u/trusted"), &values)
                .expect("second call")
        );
        assert!(
            !store
                .derive_project(
                    Path::new("/home/u/trusted"),
                    Path::new("/home/u/trusted"),
                    &["enabledMcpjsonServers"],
                )
                .expect("deriving onto itself")
        );
        // Empty values still grant: the mirror of a root with no approvals
        // needs the trust key and nothing else.
        assert!(
            store
                .grant_with_keys(Path::new("/w/fresh"), &serde_json::Map::new())
                .expect("grants a new entry")
        );
        assert!(
            !store
                .grant_with_keys(Path::new("/w/fresh"), &serde_json::Map::new())
                .expect("already trusted")
        );
    }
}
