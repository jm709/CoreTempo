//! The session manager (spec 2026-08-27 §2, §3): every lifecycle transition
//! on one session runs under that session's `tokio::Mutex`; the `PtyManager`
//! does the process work, the store remembers, the trust gate re-checks
//! before every spawn, and the bus tells everyone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use tokio::sync::broadcast;

use crate::api::auth::{TokenHint, token_matches};
use crate::api::{Caller, Roster, RosterFuture, TokenAuth};
use crate::bus::EventBus;
use crate::pty::{InjectionQueue, McpPolicy, PtyError, PtyManager, RosterEntry};
use crate::sessions::files::{
    SessionFileInputs, SessionFiles, remove_session_files, session_files, write_session_files,
};
use crate::sessions::store::{LastState, ProjectRow, SessionRow, SessionStore, WorktreeRow};
use crate::sessions::trust::{SessionTrust, SessionTrustGate};
use crate::sessions::{SessionError, SessionsRoot, session_id, worktree};
use crate::time::Timestamp;
use crate::trust::{TrustPolicy, TrustStore, preflight};
use crate::types::agent::{AgentExit, AgentState};
use crate::types::config::{AgentConfig, PermissionPrompt};
use crate::types::event::{Event, EventPayload, LifecyclePhase};
use crate::types::id::{AgentId, ProjectId, Token};
use crate::types::session::{
    BlockedView, CreateSessionRequest, DeleteSessionResponse, ProjectView, ResumeResponse,
    SessionCounts, SessionState, SessionView, WorktreeInfo, WorktreeStatus,
};

pub struct SessionManagerInputs {
    pub root: SessionsRoot,
    pub store: SessionStore,
    pub pty: Arc<PtyManager>,
    pub bus: EventBus,
    /// The operator's `.claude.json` (explicit, so tests never touch HOME).
    pub trust_store: TrustStore,
    pub policy: TrustPolicy,
    /// The `tempo` the hooks run.
    pub tempo_bin: PathBuf,
    pub operator_token: Token,
}

type SessionLock = Arc<tokio::sync::Mutex<()>>;

pub struct SessionManager {
    root: SessionsRoot,
    store: SessionStore,
    pty: Arc<PtyManager>,
    bus: EventBus,
    trust_store: TrustStore,
    policy: TrustPolicy,
    trust: Arc<SessionTrustGate>,
    tempo_bin: PathBuf,
    operator_token: Token,
    /// One lock per session: create/stop/resume/delete and the exit watcher
    /// all take it, so a stop and a resume racing on one id serialize.
    locks: Mutex<BTreeMap<AgentId, SessionLock>>,
    /// Live hook tokens, for `TokenAuth` without a store round trip.
    hook_tokens: Mutex<BTreeMap<AgentId, Token>>,
    /// Set by [`shutdown`](SessionManager::shutdown); `create` and `resume`
    /// read it under the session lock and refuse, so no spawn can slip
    /// between the shutdown's reap and the daemon's exit.
    stopping: std::sync::atomic::AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SessionManager {
    /// Loads every stored session into the `PtyManager` roster (no spawn),
    /// registers its trust, installs the spawn gate, and starts the exit
    /// watcher. Nothing auto-resumes (spec §2).
    ///
    /// # Errors
    /// [`SessionError::Store`] / [`SessionError::Spawn`] (an `AgentExists`
    /// on a duplicate row would be a corrupt store).
    pub async fn boot(inputs: SessionManagerInputs) -> Result<Arc<SessionManager>, SessionError> {
        let SessionManagerInputs {
            root,
            store,
            pty,
            bus,
            trust_store,
            policy,
            tempo_bin,
            operator_token,
        } = inputs;
        let trust = Arc::new(SessionTrustGate::new(trust_store.clone(), policy));
        pty.set_spawn_gate(Arc::clone(&trust) as Arc<dyn crate::pty::SpawnGate>);
        let me = Arc::new(SessionManager {
            root,
            store,
            pty,
            bus: bus.clone(),
            trust_store,
            policy,
            trust,
            tempo_bin,
            operator_token,
            locks: Mutex::new(BTreeMap::new()),
            hook_tokens: Mutex::new(BTreeMap::new()),
            stopping: std::sync::atomic::AtomicBool::new(false),
        });
        let projects: BTreeMap<ProjectId, ProjectRow> = me
            .store
            .list_projects()
            .await?
            .into_iter()
            .map(|p| (p.id.clone(), p))
            .collect();
        for row in me.store.list_sessions().await? {
            let Some(project) = projects.get(&row.project) else {
                tracing::warn!(
                    session = %row.id,
                    project = %row.project,
                    "row without a project; skipped"
                );
                continue;
            };
            me.attach(&row, project)?;
        }
        tokio::spawn(watch_exits(Arc::downgrade(&me), bus.subscribe()));
        Ok(me)
    }

    #[must_use]
    pub fn pty(&self) -> &Arc<PtyManager> {
        &self.pty
    }

    /// Puts a row into the roster: trust registration, hook token, PTY handle.
    fn attach(&self, row: &SessionRow, project: &ProjectRow) -> Result<(), SessionError> {
        let files = session_files(&self.root.dir, &row.id, row.isolated_config);
        self.trust
            .register(row.id.clone(), session_trust(row, project, &files));
        lock(&self.hook_tokens).insert(row.id.clone(), row.hook_token.clone());
        self.pty
            .add_agent(row.id.clone(), roster_entry(row, &files))?;
        Ok(())
    }

    /// Reverses [`attach`](SessionManager::attach); a live process is stopped.
    async fn detach(&self, id: &AgentId) {
        if let Err(error) = self.pty.remove_agent(id).await {
            tracing::debug!(session = %id, %error, "remove_agent during detach");
        }
        self.trust.forget(id);
        lock(&self.hook_tokens).remove(id);
        lock(&self.locks).remove(id);
    }

    fn lock_for(&self, id: &AgentId) -> SessionLock {
        Arc::clone(
            lock(&self.locks)
                .entry(id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }
}

/// The spawn inputs for a session (spec §4): its cwd, launch options, no
/// prompt, no auto-`/clear`, wait-mode hooks, inherited MCP, its own token.
fn roster_entry(row: &SessionRow, files: &SessionFiles) -> RosterEntry {
    RosterEntry {
        cfg: AgentConfig {
            model: row.model.clone(),
            permission_mode: row.permission_mode.clone(),
            auto_clear: false,
            isolated_config: row.isolated_config,
            on_permission_prompt: PermissionPrompt::Wait,
            ..AgentConfig::new(row.cwd.clone(), "")
        },
        system_prompt: None,
        mcp: McpPolicy::Inherit,
        settings_path: Some(files.settings.clone()),
        config_dir: files.config_dir.clone(),
        token: Some(row.hook_token.clone()),
        resume: None,
    }
}

fn session_trust(row: &SessionRow, project: &ProjectRow, files: &SessionFiles) -> SessionTrust {
    SessionTrust {
        project_root: project.path.clone(),
        derived_worktree: row.worktree.as_ref().map(|w| w.path.clone()),
        mirror: files
            .config_dir
            .as_ref()
            .map(|dir| TrustStore::at(dir.join(".claude.json"))),
    }
}

/// Records `exited` for a live row when its process leaves on its own.
/// Under the session lock: a `stop` in progress has already taken it and
/// marks the row itself, so the watcher sees `stopped_at` set and skips.
async fn watch_exits(me: Weak<SessionManager>, mut events: broadcast::Receiver<Event>) {
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "session exit watcher lagged");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let EventPayload::AgentLifecycle {
            agent,
            phase: LifecyclePhase::Exited,
            exit,
        } = event.payload
        else {
            continue;
        };
        let Some(mgr) = me.upgrade() else {
            return;
        };
        mgr.record_exit(&agent, exit).await;
    }
}

impl SessionManager {
    /// Registers the repository containing `path` (its toplevel), named
    /// after the directory unless `name` is given.
    ///
    /// # Errors
    /// [`SessionError::Io`] (missing path), [`SessionError::NotAGitRepo`],
    /// [`SessionError::ProjectExists`], [`SessionError::Store`].
    pub async fn register_project(
        &self,
        path: &Path,
        name: Option<String>,
    ) -> Result<ProjectView, SessionError> {
        let path = std::fs::canonicalize(path).map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let root = worktree::toplevel(&path).await.map_err(|e| match e {
            worktree::GitError::Failed { stderr, .. } => SessionError::NotAGitRepo {
                path: path.clone(),
                stderr,
            },
            spawn @ worktree::GitError::Spawn { .. } => SessionError::Git(spawn),
        })?;
        if let Some(existing) = self.store.project_by_path(&root).await? {
            return Err(SessionError::ProjectExists {
                id: existing.id,
                path: root,
            });
        }
        let row = ProjectRow {
            id: ProjectId::generate(),
            name: name.unwrap_or_else(|| dir_name(&root)),
            path: root,
            created_at: Timestamp::now(),
        };
        self.store.insert_project(row.clone()).await?;
        self.bus.publish(EventPayload::ProjectRegistered {
            project: row.id.clone(),
        });
        Ok(project_view(&row))
    }

    /// # Errors
    /// [`SessionError::Store`].
    pub async fn list_projects(&self) -> Result<Vec<ProjectView>, SessionError> {
        Ok(self
            .store
            .list_projects()
            .await?
            .iter()
            .map(project_view)
            .collect())
    }

    /// # Errors
    /// [`SessionError::UnknownProject`], [`SessionError::ProjectInUse`].
    pub async fn forget_project(&self, id: &ProjectId) -> Result<(), SessionError> {
        let project = self.project(id).await?;
        let sessions = self.store.sessions_in_project(&project.id).await?;
        if sessions > 0 {
            return Err(SessionError::ProjectInUse {
                id: project.id,
                sessions,
            });
        }
        self.store.delete_project(&project.id).await?;
        self.bus.publish(EventPayload::ProjectForgotten {
            project: project.id,
        });
        Ok(())
    }

    async fn project(&self, id: &ProjectId) -> Result<ProjectRow, SessionError> {
        match self.store.get_project(id).await? {
            Some(project) => Ok(project),
            None => Err(SessionError::UnknownProject {
                id: id.clone(),
                roster: self
                    .store
                    .list_projects()
                    .await?
                    .into_iter()
                    .map(|p| p.id)
                    .collect(),
            }),
        }
    }

    async fn row(&self, id: &AgentId) -> Result<SessionRow, SessionError> {
        match self.store.get_session(id).await? {
            Some(row) => Ok(row),
            None => Err(SessionError::UnknownSession {
                id: id.clone(),
                roster: self.pty.agent_ids(),
            }),
        }
    }

    /// `PtyManager::is_live`, with the off-roster case read as "no such
    /// session". A row can outlive its handle (a rollback that could not
    /// delete the row); `SessionError::Spawn`'s "cannot spawn claude: unknown
    /// agent" would be a 500 that names the wrong problem on a `stop`,
    /// `resume` or `delete`.
    fn live(&self, id: &AgentId) -> Result<bool, SessionError> {
        match self.pty.is_live(id) {
            Ok(live) => Ok(live),
            Err(PtyError::UnknownAgent(_)) => Err(SessionError::UnknownSession {
                id: id.clone(),
                roster: self.pty.agent_ids(),
            }),
            Err(other) => Err(SessionError::Spawn(other)),
        }
    }

    /// # Errors
    /// [`SessionError::UnknownSession`], [`SessionError::Store`].
    pub async fn get(&self, id: &AgentId) -> Result<SessionView, SessionError> {
        let row = self.row(id).await?;
        let project = self.project(&row.project).await?;
        Ok(self.view(&row, &project).await)
    }

    /// Every row with runtime fields (git status per row — cheap at tens).
    ///
    /// # Errors
    /// [`SessionError::Store`].
    pub async fn list(&self) -> Result<Vec<SessionView>, SessionError> {
        let projects: BTreeMap<ProjectId, ProjectRow> = self
            .store
            .list_projects()
            .await?
            .into_iter()
            .map(|p| (p.id.clone(), p))
            .collect();
        let mut views = Vec::new();
        for row in self.store.list_sessions().await? {
            if let Some(project) = projects.get(&row.project) {
                views.push(self.view(&row, project).await);
            }
        }
        Ok(views)
    }

    /// # Errors
    /// [`SessionError::Store`].
    pub async fn counts(&self) -> Result<SessionCounts, SessionError> {
        let live = self
            .pty
            .agent_ids()
            .iter()
            .filter(|id| self.pty.is_live(id).unwrap_or(false))
            .count();
        Ok(SessionCounts {
            live,
            total: self.store.count_sessions().await?,
        })
    }

    /// The row plus what the `PtyManager` and git say right now (spec §2, §5).
    async fn view(&self, row: &SessionRow, project: &ProjectRow) -> SessionView {
        let live = self.pty.is_live(&row.id).unwrap_or(false);
        let state = if live {
            match self.pty.state(&row.id).unwrap_or(AgentState::Exited) {
                AgentState::Starting => SessionState::Starting,
                AgentState::Idle => SessionState::Idle,
                AgentState::Working => SessionState::Working,
                AgentState::Exited | AgentState::Restarting => last_state(row.last_state),
            }
        } else if row.stopped_at.is_none() {
            // Not live but never marked: the child left and the exit watcher
            // has not written yet (or lagged). The process is gone, so say so.
            SessionState::Exited
        } else {
            last_state(row.last_state)
        };
        let blocked = self
            .pty
            .blocked_since(&row.id)
            .ok()
            .flatten()
            .map(|b| BlockedView {
                tool: b.tool,
                since: Timestamp::from_unix_seconds(
                    unix_now().saturating_sub(b.since.elapsed().as_secs()),
                ),
            });
        let exit = if live {
            None
        } else {
            self.pty
                .exit(&row.id)
                .ok()
                .flatten()
                .or_else(|| row.exit.clone())
        };
        let pty_cursor = self.pty.read_ring(&row.id, None).map_or(0, |(c, _)| c.0);
        let status = worktree::status(&row.cwd).await;
        let (ahead, worktree_status) = self.worktree_fields(row, project).await;
        SessionView {
            id: row.id.clone(),
            project: row.project.clone(),
            cwd: row.cwd.display().to_string(),
            worktree: row.worktree.as_ref().map(worktree_info),
            title: row.title.clone(),
            claude_session_id: row.claude_session_id.clone(),
            model: row.model.clone(),
            permission_mode: row.permission_mode.clone(),
            isolated_config: row.isolated_config,
            prompt: row.prompt.clone(),
            created_at: row.created_at.clone(),
            stopped_at: row.stopped_at.clone(),
            state,
            blocked,
            exit,
            pty_cursor,
            branch: status.branch,
            changed_files: status.changed_files,
            ahead,
            worktree_status,
        }
    }

    /// `ahead` and `worktree_status`: a worktree still in `git worktree list`
    /// with its directory present is `Present` and is counted against its base.
    async fn worktree_fields(
        &self,
        row: &SessionRow,
        project: &ProjectRow,
    ) -> (Option<u64>, WorktreeStatus) {
        let Some(wt) = &row.worktree else {
            return (None, WorktreeStatus::None);
        };
        if wt.path.is_dir() && worktree::is_listed(&project.path, &wt.path).await {
            (
                worktree::ahead(&row.cwd, &wt.base).await,
                WorktreeStatus::Present,
            )
        } else {
            (None, WorktreeStatus::Missing)
        }
    }
}

fn last_state(last: LastState) -> SessionState {
    match last {
        LastState::Stopped => SessionState::Stopped,
        LastState::Exited => SessionState::Exited,
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn dir_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn project_view(row: &ProjectRow) -> ProjectView {
    ProjectView {
        id: row.id.clone(),
        path: row.path.display().to_string(),
        name: row.name.clone(),
        created_at: row.created_at.clone(),
    }
}

fn worktree_info(wt: &WorktreeRow) -> WorktreeInfo {
    WorktreeInfo {
        path: wt.path.display().to_string(),
        branch: wt.branch.clone(),
        base: wt.base.clone(),
    }
}

/// What `create` has made so far, so a failure can undo it (spec §2: a
/// session exists only once its process is running).
struct Staged {
    id: AgentId,
    worktree: Option<worktree::Created>,
    project_root: PathBuf,
    files_written: bool,
    attached: bool,
    row_inserted: bool,
}

impl SessionManager {
    /// Spec §2 `create`, in order: project, cwd, project-root trust, worktree,
    /// files, roster, row, spawn, prompt. Any failure through the spawn rolls
    /// everything back and returns it.
    ///
    /// # Errors
    /// [`SessionError`] for each failing step.
    pub async fn create(&self, req: CreateSessionRequest) -> Result<SessionView, SessionError> {
        let project = self.project(&req.project).await?;
        let rel = relative_cwd(&project.path, req.cwd.as_deref())?;
        preflight(&self.trust_store, [project.path.as_path()], self.policy)?;
        let mut staged = Staged {
            id: session_id(),
            worktree: None,
            project_root: project.path.clone(),
            files_written: false,
            attached: false,
            row_inserted: false,
        };
        match self
            .stage_and_spawn(&req, &project, &rel, &mut staged)
            .await
        {
            Ok(row) => {
                self.bus.publish(EventPayload::SessionCreated {
                    agent: row.id.clone(),
                });
                if let Some(prompt) = &row.prompt {
                    self.inject_once(&row.id, prompt.clone());
                }
                Ok(self.view(&row, &project).await)
            }
            Err(error) => {
                self.rollback(staged).await;
                Err(error)
            }
        }
    }

    async fn stage_and_spawn(
        &self,
        req: &CreateSessionRequest,
        project: &ProjectRow,
        rel: &Path,
        staged: &mut Staged,
    ) -> Result<SessionRow, SessionError> {
        if req.worktree {
            staged.worktree =
                Some(worktree::create(&project.path, &self.root.worktrees, &project.id).await?);
        }
        let base = staged
            .worktree
            .as_ref()
            .map_or(project.path.clone(), |w| w.path.clone());
        // `join("")` would leave a trailing separator on the root itself,
        // which then reaches the spawn cwd, the row and every view.
        let cwd = if rel.as_os_str().is_empty() {
            base
        } else {
            base.join(rel)
        };
        if !cwd.is_dir() {
            return Err(SessionError::CwdMissing { cwd });
        }
        let id = staged.id.clone();
        let files = write_session_files(&SessionFileInputs {
            root: &self.root.dir,
            id: &id,
            tempo_bin: &self.tempo_bin,
            isolated_config: req.isolated_config,
        })?;
        staged.files_written = true;
        let row = SessionRow {
            id: id.clone(),
            project: project.id.clone(),
            cwd: cwd.clone(),
            worktree: staged.worktree.as_ref().map(|w| WorktreeRow {
                path: w.path.clone(),
                branch: w.branch.clone(),
                base: w.base.clone(),
            }),
            title: title_for(req, staged.worktree.as_ref(), &cwd),
            claude_session_id: None,
            model: req.model.clone(),
            permission_mode: req.permission_mode.clone(),
            isolated_config: req.isolated_config,
            prompt: req.prompt.clone(),
            hook_token: Token::generate(),
            last_state: LastState::Exited,
            exit: None,
            created_at: Timestamp::now(),
            stopped_at: None,
        };
        self.trust
            .register(id.clone(), session_trust(&row, project, &files));
        lock(&self.hook_tokens).insert(id.clone(), row.hook_token.clone());
        self.pty.add_agent(id.clone(), roster_entry(&row, &files))?;
        staged.attached = true;
        // The row precedes the spawn: the SessionStart hook's claude_session_id
        // needs somewhere to land, and it can fire before spawn() returns.
        self.store.insert_session(row.clone()).await?;
        staged.row_inserted = true;
        let session_lock = self.lock_for(&id);
        let _held = session_lock.lock().await;
        if self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(SessionError::ShuttingDown);
        }
        self.pty.spawn(&id).await?;
        Ok(row)
    }

    async fn rollback(&self, staged: Staged) {
        let id = staged.id;
        if staged.row_inserted
            && let Err(error) = self.store.delete_session(&id).await
        {
            // The files and the handle are gone but the row survives: nothing
            // can reach it again (`is_live` fails `UnknownAgent`), so say so.
            tracing::error!(session = %id, %error, "could not remove the session row on rollback");
        }
        if staged.attached {
            self.detach(&id).await;
        }
        if staged.files_written
            && let Err(error) = remove_session_files(&self.root.dir, &id)
        {
            tracing::warn!(session = %id, %error, "could not remove session files on rollback");
        }
        if let Some(wt) = staged.worktree {
            // Nothing has run in it: force is safe, and the branch is unmoved.
            if let Err(error) = worktree::remove(&staged.project_root, &wt.path, true).await {
                tracing::warn!(session = %id, %error, "could not remove the worktree on rollback");
            }
            if let Err(error) =
                worktree::delete_branch_if_unmoved(&staged.project_root, &wt.branch, &wt.base).await
            {
                tracing::warn!(session = %id, %error, "could not delete the branch on rollback");
            }
        }
    }

    /// The first turn, typed once through the queue's submit verification;
    /// a session that exits first fails it, which is logged and nothing else.
    fn inject_once(&self, id: &AgentId, prompt: String) {
        let rx = InjectionQueue::enqueue(self.pty.as_ref(), id.clone(), prompt);
        let id = id.clone();
        tokio::spawn(async move {
            match rx.await {
                Ok(Ok(_)) => tracing::debug!(session = %id, "first prompt injected"),
                Ok(Err(error)) => {
                    tracing::warn!(session = %id, %error, "first prompt was not injected");
                }
                Err(_) => tracing::warn!(session = %id, "first prompt injection dropped"),
            }
        });
    }
}

/// `cwd` as a path relative to the project root: empty for the root itself.
fn relative_cwd(root: &Path, cwd: Option<&str>) -> Result<PathBuf, SessionError> {
    let Some(cwd) = cwd else {
        return Ok(PathBuf::new());
    };
    let requested = Path::new(cwd);
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let canonical = std::fs::canonicalize(&absolute).map_err(|source| SessionError::Io {
        path: absolute.clone(),
        source,
    })?;
    canonical
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| SessionError::CwdOutsideProject {
            cwd: canonical.clone(),
            root: root.to_path_buf(),
        })
}

/// Spec §2: title, else the prompt's first line, else the branch, else the dir.
fn title_for(req: &CreateSessionRequest, wt: Option<&worktree::Created>, cwd: &Path) -> String {
    if let Some(title) = req.title.as_deref().filter(|t| !t.trim().is_empty()) {
        return title.to_string();
    }
    if let Some(line) = req
        .prompt
        .as_deref()
        .and_then(|p| p.lines().next())
        .filter(|l| !l.trim().is_empty())
    {
        return line.to_string();
    }
    match wt {
        Some(wt) => wt.branch.clone(),
        None => dir_name(cwd),
    }
}

impl SessionManager {
    /// # Errors
    /// [`SessionError::WrongState`] unless live.
    pub async fn stop(&self, id: &AgentId) -> Result<SessionView, SessionError> {
        let session_lock = self.lock_for(id);
        let _held = session_lock.lock().await;
        let row = self.row(id).await?;
        let project = self.project(&row.project).await?;
        if !self.live(id)? {
            return Err(self
                .wrong_state(&row, &project, "stop", "resume or delete")
                .await);
        }
        // The child can leave on its own between `is_live` and here; that is
        // the spec's `exit` transition, not an error — record it as such.
        let last = match self.pty.stop(id).await {
            Ok(()) => LastState::Stopped,
            Err(PtyError::AgentExited(_)) => LastState::Exited,
            Err(other) => return Err(SessionError::Spawn(other)),
        };
        let exit = self.pty.exit(id)?;
        self.store
            .mark_left_live(id, last, exit, Timestamp::now())
            .await?;
        self.bus
            .publish(EventPayload::SessionStopped { agent: id.clone() });
        let row = self.row(id).await?;
        Ok(self.view(&row, &project).await)
    }

    /// # Errors
    /// [`SessionError::WrongState`] while live, [`SessionError::WorktreeMissing`],
    /// [`SessionError::Spawn`] (the row is left as it was).
    pub async fn resume(&self, id: &AgentId) -> Result<ResumeResponse, SessionError> {
        let session_lock = self.lock_for(id);
        let _held = session_lock.lock().await;
        let row = self.row(id).await?;
        let project = self.project(&row.project).await?;
        if self.live(id)? {
            return Err(self.wrong_state(&row, &project, "resume", "stop").await);
        }
        if let Some(wt) = &row.worktree
            && !(wt.path.is_dir() && worktree::is_listed(&project.path, &wt.path).await)
        {
            return Err(SessionError::WorktreeMissing {
                id: id.clone(),
                path: wt.path.clone(),
            });
        }
        if self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(SessionError::ShuttingDown);
        }
        // The project root's trust is re-checked here, not only in the spawn
        // gate: a refusal must surface as `Trust` (409 `untrusted`, roots and
        // both fixes), and the gate's `Err(String)` would arrive as
        // `PtyError::Spawn` → 500. The gate still runs (derived worktree key,
        // mirrors) once this passes.
        preflight(&self.trust_store, [project.path.as_path()], self.policy)?;
        let resumed = row.claude_session_id.is_some();
        // Set right before the spawn, under the lock: a stale id never rides
        // along, and a refused spawn leaves it armed only until the next resume
        // sets it again from the row.
        self.pty.set_resume(id, row.claude_session_id.clone())?;
        self.pty.spawn(id).await?;
        self.store.mark_live(id).await?;
        self.bus.publish(EventPayload::SessionResumed {
            agent: id.clone(),
            resumed,
        });
        let row = self.row(id).await?;
        Ok(ResumeResponse {
            session: self.view(&row, &project).await,
            resumed,
        })
    }

    /// # Errors
    /// [`SessionError::WrongState`] while live, [`SessionError::Dirty`]
    /// (nothing touched), git and store errors.
    pub async fn delete(
        &self,
        id: &AgentId,
        remove_worktree: bool,
        force: bool,
    ) -> Result<DeleteSessionResponse, SessionError> {
        let session_lock = self.lock_for(id);
        let _held = session_lock.lock().await;
        let row = self.row(id).await?;
        let project = self.project(&row.project).await?;
        if self.live(id)? {
            return Err(self.wrong_state(&row, &project, "delete", "stop").await);
        }
        let mut branch_kept = false;
        if remove_worktree && let Some(wt) = &row.worktree {
            if wt.path.exists() {
                worktree::remove(&project.path, &wt.path, force).await?;
            } else {
                worktree::prune(&project.path).await?;
            }
            branch_kept =
                !worktree::delete_branch_if_unmoved(&project.path, &wt.branch, &wt.base).await?;
        }
        // Persistent state first, the handle last: anything here can fail,
        // and a row that has already lost its handle cannot be deleted again.
        remove_session_files(&self.root.dir, id).map_err(|source| SessionError::Io {
            path: self.root.dir.join(&id.0),
            source,
        })?;
        self.store.delete_session(id).await?;
        self.detach(id).await;
        self.bus
            .publish(EventPayload::SessionDeleted { agent: id.clone() });
        Ok(DeleteSessionResponse { branch_kept })
    }

    async fn wrong_state(
        &self,
        row: &SessionRow,
        project: &ProjectRow,
        action: &'static str,
        valid: &'static str,
    ) -> SessionError {
        SessionError::WrongState {
            id: row.id.clone(),
            state: self.view(row, project).await.state,
            action,
            valid,
        }
    }

    /// The exit watcher's write (see [`watch_exits`]).
    async fn record_exit(&self, id: &AgentId, exit: Option<AgentExit>) {
        let session_lock = self.lock_for(id);
        let _held = session_lock.lock().await;
        if self.pty.is_live(id).unwrap_or(false) {
            return; // resumed already
        }
        let row = match self.store.get_session(id).await {
            Ok(Some(row)) => row,
            Ok(None) => return, // deleted under us
            Err(error) => {
                tracing::error!(session = %id, %error, "could not read the row to record an exit");
                return;
            }
        };
        if row.stopped_at.is_some() {
            return; // stop() marked it
        }
        if let Err(error) = self
            .store
            .mark_left_live(id, LastState::Exited, exit, Timestamp::now())
            .await
        {
            tracing::error!(session = %id, %error, "could not record the session's exit");
        }
    }

    /// A `SessionStart` hook reported its Claude Code session id; latest wins.
    pub async fn record_claude_session_id(&self, id: &AgentId, sid: String) {
        match self.store.set_claude_session_id(id, sid).await {
            Ok(true) => {}
            Ok(false) => tracing::debug!(session = %id, "claude_session_id for an unknown row"),
            Err(error) => {
                tracing::error!(session = %id, %error, "could not store claude_session_id");
            }
        }
    }

    /// Daemon shutdown (spec §2): every process stopped, every live row
    /// `exited` with `stopped_at`. Nothing auto-resumes. Holds every
    /// session's lock across the reap and sets `stopping` first, so an
    /// in-flight `create`/`resume` either finished before this (and is
    /// reaped here) or refuses `ShuttingDown` after it — never a spawn that
    /// lands between the reap and the exit (`PtyManager::spawn` has two lock
    /// sections; `pty.shutdown` alone could fall between them).
    pub async fn shutdown(&self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let locks: Vec<SessionLock> = self
            .pty
            .agent_ids()
            .iter()
            .map(|id| self.lock_for(id))
            .collect();
        let mut held = Vec::with_capacity(locks.len());
        for session_lock in &locks {
            held.push(session_lock.lock().await);
        }
        self.pty.shutdown().await;
        drop(held);
        match self.store.mark_all_left_live(Timestamp::now()).await {
            Ok(n) => tracing::info!(sessions = n, "sessions marked exited at shutdown"),
            Err(error) => tracing::error!(%error, "could not mark sessions at shutdown"),
        }
    }
}

impl Roster for SessionManager {
    fn contains(&self, id: &AgentId) -> bool {
        self.pty.is_live(id).is_ok()
    }
    fn ids(&self) -> Vec<AgentId> {
        self.pty.agent_ids()
    }
    fn on_claude_session_id<'a>(&'a self, id: &'a AgentId, session_id: String) -> RosterFuture<'a> {
        Box::pin(self.record_claude_session_id(id, session_id))
    }
}

impl TokenAuth for SessionManager {
    /// The operator token, then every hook token — all compared, no early
    /// exit, so timing says nothing about which one matched.
    fn classify(&self, bearer: &str) -> Caller {
        let mut found = if token_matches(&self.operator_token, bearer) {
            Caller::Operator
        } else {
            Caller::Unknown
        };
        for (id, token) in lock(&self.hook_tokens).iter() {
            if token_matches(token, bearer) && found == Caller::Unknown {
                found = Caller::Hook(id.clone());
            }
        }
        found
    }
    fn hint(&self) -> TokenHint {
        TokenHint::Sessions
    }
}
