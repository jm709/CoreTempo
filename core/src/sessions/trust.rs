//! Trust for sessions (spec 2026-08-27 §3). A session's `dir` is either a
//! directory in a registered repository — checked exactly as a workflow
//! agent's — or inside a worktree `CoreTempo` made, whose trust is *derived*:
//! if the operator trusted the project root, the worktree gets the key on
//! every spawn (a live Claude flush can revert it), and never otherwise.
//! The same grant copies the root's `.mcp.json` approvals so
//! `McpPolicy::Inherit` raises no "New MCP server found" dialog in the
//! fresh path. An `isolated_config` session's managed `.claude.json` mirrors
//! both — never a second consent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::pty::SpawnGate;
use crate::trust::{TrustPolicy, TrustStore, preflight, trust_root};
use crate::types::id::AgentId;

/// The `projects[<root>]` keys Claude Code decides `.mcp.json` approvals by.
/// Verify against the running Claude Code when it changes (`./dev live`).
pub const MCP_APPROVAL_KEYS: [&str; 3] = [
    "enabledMcpjsonServers",
    "disabledMcpjsonServers",
    "enableAllProjectMcpServers",
];

/// What the gate knows about one session.
#[derive(Debug, Clone)]
pub struct SessionTrust {
    /// The registered repository root (canonical).
    pub project_root: PathBuf,
    /// The worktree `CoreTempo` created for this session, if any.
    pub derived_worktree: Option<PathBuf>,
    /// The managed `.claude.json` of an `isolated_config` session.
    pub mirror: Option<TrustStore>,
}

/// The sessions daemon's [`SpawnGate`]: registrations come and go with
/// sessions, unlike `TrustGate`'s fixed mirror map.
pub struct SessionTrustGate {
    store: TrustStore,
    policy: TrustPolicy,
    sessions: Mutex<BTreeMap<AgentId, SessionTrust>>,
}

impl SessionTrustGate {
    #[must_use]
    pub fn new(store: TrustStore, policy: TrustPolicy) -> SessionTrustGate {
        SessionTrustGate {
            store,
            policy,
            sessions: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn register(&self, id: AgentId, trust: SessionTrust) {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, trust);
    }

    pub fn forget(&self, id: &AgentId) {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
    }

    fn lookup(&self, id: &AgentId) -> Option<SessionTrust> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    /// The derived rule: operator consent for the repository, then the key
    /// and the MCP approvals written for the worktree.
    fn derive(&self, agent: &AgentId, trust: &SessionTrust, worktree: &Path) -> Result<(), String> {
        preflight(&self.store, [trust.project_root.as_path()], self.policy)
            .map_err(|e| e.to_string())?;
        let wt_root = trust_root(worktree);
        self.store
            .grant(std::slice::from_ref(&wt_root))
            .map_err(|e| e.to_string())?;
        let approvals = self
            .store
            .project_keys(&trust.project_root, &MCP_APPROVAL_KEYS)
            .map_err(|e| e.to_string())?;
        self.store
            .set_project_keys(&wt_root, approvals)
            .map_err(|e| e.to_string())?;
        tracing::info!(
            agent = %agent,
            worktree = %wt_root.display(),
            project = %trust.project_root.display(),
            "granted derived trust for a session worktree (the project root is trusted)"
        );
        Ok(())
    }

    fn mirror(&self, mirror: &TrustStore, root: &Path) -> Result<(), String> {
        mirror
            .grant(&[root.to_path_buf()])
            .map_err(|e| format!("cannot mirror trust into the managed config dir: {e}"))?;
        let approvals = self
            .store
            .project_keys(root, &MCP_APPROVAL_KEYS)
            .map_err(|e| e.to_string())?;
        mirror
            .set_project_keys(root, approvals)
            .map_err(|e| format!("cannot mirror MCP approvals into the managed config dir: {e}"))
    }
}

impl SpawnGate for SessionTrustGate {
    fn before_spawn(&self, agent: &AgentId, dir: &Path) -> Result<(), String> {
        let Some(trust) = self.lookup(agent) else {
            return Err(format!(
                "session '{agent}' has no trust registration; this is a CoreTempo bug — \
                 delete the session and create it again"
            ));
        };
        if let Some(worktree) = &trust.derived_worktree {
            self.derive(agent, &trust, worktree)?;
        } else {
            let granted = preflight(&self.store, [dir], self.policy).map_err(|e| e.to_string())?;
            if !granted.is_empty() {
                tracing::warn!(
                    agent = %agent,
                    "trust key was missing right before spawn (reverted by a live \
                     Claude session?); re-granted"
                );
            }
        }
        if let Some(mirror) = &trust.mirror {
            self.mirror(mirror, &trust_root(dir))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use crate::pty::SpawnGate;
    use crate::sessions::trust::{SessionTrust, SessionTrustGate};
    use crate::trust::{TrustPolicy, TrustStore, trust_root};
    use crate::types::id::AgentId;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tmpdir")
    }

    /// A main repo (`.git` dir) and a linked worktree (`.git` file).
    fn repo_and_worktree(t: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let repo = t.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("repo");
        let wt = t.path().join("wt");
        std::fs::create_dir_all(wt.join("src")).expect("wt");
        std::fs::write(wt.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").expect("file");
        (trust_root(&repo), trust_root(&wt))
    }

    fn operator(t: &tempfile::TempDir, trusted: &Path) -> TrustStore {
        let store = TrustStore::at(t.path().join("claude.json"));
        store.grant(&[trusted.to_path_buf()]).expect("grant");
        store
            .set_project_keys(
                trusted,
                json!({"enabledMcpjsonServers": ["mailbox"]})
                    .as_object()
                    .cloned()
                    .expect("obj"),
            )
            .expect("approvals");
        store
    }

    fn read(store: &TrustStore) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(&store.path).expect("read")).expect("json")
    }

    #[test]
    fn a_trusted_project_root_derives_trust_and_approvals_for_its_worktree() {
        let t = tmp();
        let (repo, wt) = repo_and_worktree(&t);
        let store = operator(&t, &repo);
        let gate = SessionTrustGate::new(store.clone(), TrustPolicy { grant: false });
        let id = AgentId("s-1".into());
        gate.register(
            id.clone(),
            SessionTrust {
                project_root: repo.clone(),
                derived_worktree: Some(wt.clone()),
                mirror: None,
            },
        );
        gate.before_spawn(&id, &wt.join("src"))
            .expect("derived grant");
        let doc = read(&store);
        let entry = &doc["projects"][wt.to_string_lossy().as_ref()];
        assert_eq!(entry["hasTrustDialogAccepted"], true);
        assert_eq!(entry["enabledMcpjsonServers"], json!(["mailbox"]));
        // A live Claude flush reverted the key; the next spawn re-derives it.
        let mut reverted = doc.clone();
        reverted["projects"][wt.to_string_lossy().as_ref()]["hasTrustDialogAccepted"] =
            json!(false);
        std::fs::write(&store.path, reverted.to_string()).expect("revert");
        gate.before_spawn(&id, &wt).expect("re-derived");
        assert_eq!(
            read(&store)["projects"][wt.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn an_untrusted_project_root_refuses_the_worktree_spawn_naming_both_fixes() {
        let t = tmp();
        let (repo, wt) = repo_and_worktree(&t);
        let store = TrustStore::at(t.path().join("claude.json"));
        let gate = SessionTrustGate::new(store.clone(), TrustPolicy { grant: false });
        let id = AgentId("s-1".into());
        gate.register(
            id.clone(),
            SessionTrust {
                project_root: repo.clone(),
                derived_worktree: Some(wt.clone()),
                mirror: None,
            },
        );
        let reason = gate.before_spawn(&id, &wt).expect_err("refused");
        assert!(reason.contains(&repo.display().to_string()), "{reason}");
        assert!(reason.contains("trust_agent_dirs = true"), "{reason}");
        assert!(!store.path.exists(), "nothing written");
    }

    #[test]
    fn a_plain_session_is_checked_like_a_workflow_agent_and_a_mirror_gets_both_keys() {
        let t = tmp();
        let (repo, _) = repo_and_worktree(&t);
        let store = operator(&t, &repo);
        let managed = t.path().join("claude-config").join(".claude.json");
        std::fs::create_dir_all(managed.parent().expect("parent")).expect("mk");
        std::fs::write(&managed, r#"{"hasCompletedOnboarding":true}"#).expect("seed");
        let gate = SessionTrustGate::new(store, TrustPolicy { grant: false });
        let id = AgentId("s-2".into());
        gate.register(
            id.clone(),
            SessionTrust {
                project_root: repo.clone(),
                derived_worktree: None,
                mirror: Some(TrustStore::at(managed.clone())),
            },
        );
        gate.before_spawn(&id, &repo).expect("trusted root");
        let mirror = read(&TrustStore::at(managed));
        let entry = &mirror["projects"][repo.to_string_lossy().as_ref()];
        assert_eq!(entry["hasTrustDialogAccepted"], true);
        assert_eq!(entry["enabledMcpjsonServers"], json!(["mailbox"]));
        assert_eq!(mirror["hasCompletedOnboarding"], true);
    }

    #[test]
    fn an_unregistered_session_cannot_spawn() {
        let t = tmp();
        let gate = SessionTrustGate::new(
            TrustStore::at(t.path().join("claude.json")),
            TrustPolicy { grant: true },
        );
        let reason = gate
            .before_spawn(&AgentId("s-9".into()), t.path())
            .expect_err("no registration");
        assert!(reason.contains("s-9"), "{reason}");
        gate.forget(&AgentId("s-9".into()));
    }
}
