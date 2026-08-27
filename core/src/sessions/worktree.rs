//! Git worktrees for sessions (spec 2026-08-27 §5): created outside the
//! repository under `~/.coretempo/worktrees/<project-id>/<slug>` on a
//! `session/<slug>` branch, inspected on every `GET`, removed on delete.
//! Every git call runs through `tokio::process` in the directory named.

use std::path::{Path, PathBuf};

use crate::types::id::{ProjectId, random_hex};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("cannot run git: {source}; is git installed and on PATH?")]
    Spawn {
        #[source]
        source: std::io::Error,
    },
    #[error("`{command}` failed in {}: {stderr}", dir.display())]
    Failed {
        command: String,
        dir: PathBuf,
        stderr: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(
        "worktree {} has uncommitted changes:\n{summary}\ncommit or stash them, or delete \
         with force = true to discard them",
        path.display()
    )]
    Dirty { path: PathBuf, summary: String },
}

/// What `create` made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub path: PathBuf,
    pub branch: String,
    /// `HEAD` at creation.
    pub base: String,
}

/// `git symbolic-ref --short HEAD` and the `git status --porcelain` line
/// count; `None` for whichever git could not answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    pub branch: Option<String>,
    pub changed_files: Option<u64>,
}

const ADJECTIVES: [&str; 32] = [
    "amber", "brisk", "calm", "clever", "crisp", "dapper", "eager", "fair", "gentle", "glad",
    "hardy", "humble", "jolly", "keen", "kind", "lively", "lucky", "merry", "mild", "nimble",
    "noble", "plucky", "proud", "quick", "quiet", "rapid", "sharp", "snug", "steady", "sunny",
    "swift", "witty",
];
const NOUNS: [&str; 32] = [
    "badger", "bison", "crane", "dingo", "egret", "falcon", "gecko", "heron", "ibis", "jackal",
    "koala", "lemur", "marmot", "newt", "ocelot", "otter", "panda", "quail", "raven", "robin",
    "salmon", "shrew", "stoat", "tapir", "toucan", "urchin", "viper", "walrus", "weasel", "wren",
    "yak", "zebra",
];

/// `brisk-otter-3f1a`: readable, and unique enough that a collision is a
/// retry, not a failure.
#[must_use]
pub fn slug() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let adjective = ADJECTIVES[rng.random_range(0..ADJECTIVES.len())];
    let noun = NOUNS[rng.random_range(0..NOUNS.len())];
    format!("{adjective}-{noun}-{}", random_hex(2))
}

async fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
    tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .map_err(|source| GitError::Spawn { source })
}

/// Runs git and returns trimmed stdout, or the command and stderr on failure.
async fn git_ok(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = git(dir, args).await?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    Err(GitError::Failed {
        command: format!("git {}", args.join(" ")),
        dir: dir.to_path_buf(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// The repository root containing `dir`, canonical.
///
/// # Errors
/// [`GitError::Failed`] with git's "not a git repository" when it is not one.
pub async fn toplevel(dir: &Path) -> Result<PathBuf, GitError> {
    let top = git_ok(dir, &["rev-parse", "--show-toplevel"]).await?;
    let top = PathBuf::from(top);
    Ok(std::fs::canonicalize(&top).unwrap_or(top))
}

/// `git worktree add -b session/<slug> <worktrees_dir>/<project>/<slug> HEAD`
/// in `root`; a branch-name collision picks a new slug (at most 5 tries).
///
/// # Errors
/// [`GitError`] with the command and git's stderr.
pub async fn create(
    root: &Path,
    worktrees_dir: &Path,
    project: &ProjectId,
) -> Result<Created, GitError> {
    let project_dir = worktrees_dir.join(&project.0);
    std::fs::create_dir_all(&project_dir).map_err(|source| GitError::Spawn { source })?;
    let mut last = None;
    for _ in 0..5 {
        let slug = slug();
        let branch = format!("session/{slug}");
        let path = project_dir.join(&slug);
        let path_arg = path.to_string_lossy().into_owned();
        match git_ok(root, &["worktree", "add", "-b", &branch, &path_arg, "HEAD"]).await {
            Ok(_) => {
                let base = git_ok(&path, &["rev-parse", "HEAD"]).await?;
                return Ok(Created { path, branch, base });
            }
            Err(GitError::Failed {
                stderr,
                command,
                dir,
            }) if stderr.contains("already exists") => {
                last = Some(GitError::Failed {
                    stderr,
                    command,
                    dir,
                });
            }
            Err(other) => return Err(other),
        }
    }
    Err(last.unwrap_or(GitError::Spawn {
        source: std::io::Error::other("no slug attempt ran"),
    }))
}

/// Branch and dirty-file count for `cwd`; git failures read as `None`.
pub async fn status(cwd: &Path) -> Status {
    if !cwd.is_dir() {
        return Status::default();
    }
    let branch = git_ok(cwd, &["symbolic-ref", "--short", "-q", "HEAD"])
        .await
        .ok()
        .filter(|b| !b.is_empty());
    let changed_files = git_ok(cwd, &["status", "--porcelain"])
        .await
        .ok()
        .map(|s| u64::try_from(s.lines().filter(|l| !l.is_empty()).count()).unwrap_or(u64::MAX));
    Status {
        branch,
        changed_files,
    }
}

/// `git rev-list --count <base>..HEAD` in `cwd`.
pub async fn ahead(cwd: &Path, base: &str) -> Option<u64> {
    git_ok(cwd, &["rev-list", "--count", &format!("{base}..HEAD")])
        .await
        .ok()?
        .parse()
        .ok()
}

/// Whether `git worktree list` in `root` still names `path` (a worktree whose
/// directory was deleted stays listed until pruned).
pub async fn is_listed(root: &Path, path: &Path) -> bool {
    let Ok(listing) = git_ok(root, &["worktree", "list", "--porcelain"]).await else {
        return false;
    };
    let wanted = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    listing
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|listed| {
            let listed = Path::new(listed);
            listed == wanted || listed == path
        })
}

/// `git worktree remove [--force] <path>`; without `force` a dirty tree is
/// refused with its porcelain summary before anything is touched.
///
/// # Errors
/// [`WorktreeError::Dirty`], or [`WorktreeError::Git`] from git.
pub async fn remove(root: &Path, path: &Path, force: bool) -> Result<(), WorktreeError> {
    if !force {
        let summary = git_ok(path, &["status", "--porcelain"]).await?;
        if !summary.is_empty() {
            return Err(WorktreeError::Dirty {
                path: path.to_path_buf(),
                summary,
            });
        }
    }
    let path_arg = path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path_arg);
    git_ok(root, &args).await?;
    Ok(())
}

/// `git worktree prune` — forgets worktrees whose directories are gone.
///
/// # Errors
/// [`GitError`].
pub async fn prune(root: &Path) -> Result<(), GitError> {
    git_ok(root, &["worktree", "prune"]).await.map(|_| ())
}

/// `git branch -D <branch>` only when `branch` is an ancestor of `base` —
/// i.e. it has no commits of its own. Returns whether it was deleted.
///
/// # Errors
/// [`GitError`] when git itself fails (a missing branch counts as failure).
pub async fn delete_branch_if_unmoved(
    root: &Path,
    branch: &str,
    base: &str,
) -> Result<bool, GitError> {
    let out = git(root, &["merge-base", "--is-ancestor", branch, base]).await?;
    match out.status.code() {
        Some(0) => {
            git_ok(root, &["branch", "-D", branch]).await?;
            Ok(true)
        }
        Some(1) => Ok(false),
        _ => Err(GitError::Failed {
            command: format!("git merge-base --is-ancestor {branch} {base}"),
            dir: root.to_path_buf(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        }),
    }
}
