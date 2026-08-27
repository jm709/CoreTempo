// No `clippy::panic` expectation: the only `panic!`s are inside `#[tokio::test]`
// fns, which `allow-panic-in-tests` covers; an unfulfilled `expect` warns.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns are not covered by allow-*-in-tests"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use coretempo_core::sessions::worktree::{self, GitError, WorktreeError};
use coretempo_core::types::ProjectId;

/// A repo with one commit; git needs an identity to commit, given inline so
/// the developer's global config is never read.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn repo(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("coretempo-wt-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "hi\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    (
        std::fs::canonicalize(&repo).unwrap(),
        root.join("worktrees"),
    )
}

fn pid() -> ProjectId {
    ProjectId("p-0a1b2c3d".into())
}

#[test]
fn slugs_are_two_words_and_four_hex() {
    let slug = worktree::slug();
    let parts: Vec<&str> = slug.split('-').collect();
    assert_eq!(parts.len(), 3, "{slug}");
    assert!(parts[0].chars().all(|c| c.is_ascii_lowercase()));
    assert!(parts[1].chars().all(|c| c.is_ascii_lowercase()));
    assert_eq!(parts[2].len(), 4);
    assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(worktree::slug(), slug);
}

#[tokio::test]
async fn create_puts_the_worktree_outside_the_repo_on_a_session_branch() {
    let (repo, worktrees) = repo("create");
    let created = worktree::create(&repo, &worktrees, &pid()).await.unwrap();
    assert!(
        created.path.starts_with(worktrees.join("p-0a1b2c3d")),
        "{}",
        created.path.display()
    );
    assert!(!created.path.starts_with(&repo));
    assert_eq!(
        created.branch,
        format!(
            "session/{}",
            created.path.file_name().unwrap().to_string_lossy()
        )
    );
    assert_eq!(created.base, git(&repo, &["rev-parse", "HEAD"]));
    assert_eq!(
        git(&created.path, &["symbolic-ref", "--short", "HEAD"]),
        created.branch
    );
    assert!(worktree::is_listed(&repo, &created.path).await);
    assert_eq!(
        worktree::toplevel(&created.path.join("")).await.unwrap(),
        std::fs::canonicalize(&created.path).unwrap()
    );
}

#[tokio::test]
async fn create_in_a_non_repo_names_the_command_and_stderr() {
    // The slug-collision retry has no deterministic trigger without injecting
    // the generator (not worth a seam); this proves the failure path instead:
    // a bare directory is not a repository, and the error carries the command
    // and git's stderr as spec §8 requires.
    let root = std::env::temp_dir().join(format!("coretempo-wt-{}-norepo", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let err = worktree::create(&root, &root.join("wt"), &pid())
        .await
        .unwrap_err();
    let GitError::Failed {
        command, stderr, ..
    } = &err
    else {
        panic!("expected Failed, got {err:?}");
    };
    assert!(command.starts_with("git worktree add"), "{command}");
    assert!(stderr.contains("not a git repository"), "{stderr}");
}

#[tokio::test]
async fn status_reports_branch_and_changed_files() {
    let (repo, _) = repo("status");
    let clean = worktree::status(&repo).await;
    assert_eq!(clean.branch.as_deref(), Some("main"));
    assert_eq!(clean.changed_files, Some(0));
    std::fs::write(repo.join("a.txt"), "a").unwrap();
    std::fs::write(repo.join("README"), "changed").unwrap();
    let dirty = worktree::status(&repo).await;
    assert_eq!(dirty.changed_files, Some(2));
    let nowhere = worktree::status(Path::new("/nonexistent/coretempo")).await;
    assert_eq!((nowhere.branch, nowhere.changed_files), (None, None));
}

#[tokio::test]
async fn ahead_counts_commits_past_the_base() {
    let (repo, worktrees) = repo("ahead");
    let created = worktree::create(&repo, &worktrees, &pid()).await.unwrap();
    assert_eq!(worktree::ahead(&created.path, &created.base).await, Some(0));
    std::fs::write(created.path.join("b.txt"), "b").unwrap();
    git(&created.path, &["add", "."]);
    git(&created.path, &["commit", "-q", "-m", "b"]);
    assert_eq!(worktree::ahead(&created.path, &created.base).await, Some(1));
    assert_eq!(worktree::ahead(&created.path, "not-a-commit").await, None);
}

#[tokio::test]
async fn remove_refuses_a_dirty_tree_unless_forced_and_keeps_a_moved_branch() {
    let (repo, worktrees) = repo("remove");
    let created = worktree::create(&repo, &worktrees, &pid()).await.unwrap();
    std::fs::write(created.path.join("wip.txt"), "wip").unwrap();
    let err = worktree::remove(&repo, &created.path, false)
        .await
        .unwrap_err();
    let WorktreeError::Dirty { summary, .. } = &err else {
        panic!("expected Dirty, got {err:?}");
    };
    assert!(summary.contains("wip.txt"), "{summary}");
    assert!(created.path.exists(), "nothing removed on refusal");
    git(&created.path, &["add", "."]);
    git(&created.path, &["commit", "-q", "-m", "wip"]);
    worktree::remove(&repo, &created.path, true).await.unwrap();
    assert!(!created.path.exists());
    assert!(!worktree::is_listed(&repo, &created.path).await);
    let deleted = worktree::delete_branch_if_unmoved(&repo, &created.branch, &created.base)
        .await
        .unwrap();
    assert!(!deleted, "a branch with its own commit is kept");
    assert_eq!(
        git(&repo, &["branch", "--list", &created.branch])
            .trim_start_matches("* ")
            .trim(),
        created.branch
    );
}

#[tokio::test]
async fn an_unmoved_branch_is_deleted_and_a_missing_worktree_is_pruned() {
    let (repo, worktrees) = repo("prune");
    let created = worktree::create(&repo, &worktrees, &pid()).await.unwrap();
    std::fs::remove_dir_all(&created.path).unwrap();
    assert!(
        worktree::is_listed(&repo, &created.path).await,
        "listed until pruned"
    );
    worktree::prune(&repo).await.unwrap();
    assert!(!worktree::is_listed(&repo, &created.path).await);
    assert!(
        worktree::delete_branch_if_unmoved(&repo, &created.branch, &created.base)
            .await
            .unwrap()
    );
    assert_eq!(git(&repo, &["branch", "--list", &created.branch]), "");
}
