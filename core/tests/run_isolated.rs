//! The managed Claude config dir for `isolated_config` agents (spec 2026-08-24).
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns are not covered by allow-*-in-tests"
)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use coretempo_core::claude_config::SETTINGS_JSON;
use coretempo_core::run::{RunError, RunOptions};
use coretempo_core::trust::{TrustPolicy, TrustStore, trust_root};
use support::run::RunScaffold;

/// On top of the shared scaffold: `~/.claude/.credentials.json` exists, the
/// fake `claude` records its `CLAUDE_CONFIG_DIR` and
/// `CLAUDE_SECURESTORAGE_CONFIG_DIR` (one per line) in `<root>/seen-<agent>`,
/// and the workflow has one isolated agent `iso` (skill `./skills/handoff`)
/// and one plain agent `plain`, both in `<root>/agent`.
async fn scaffold(name: &str) -> RunScaffold {
    let scaffold = RunScaffold::new(name).await;
    let root = &scaffold.root;
    let skill = root.join("skills").join("handoff");
    std::fs::create_dir_all(scaffold.home.join(".claude")).unwrap();
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        scaffold.home.join(".claude").join(".credentials.json"),
        "{}",
    )
    .unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: handoff\n---\n").unwrap();
    scaffold.fake_claude(&format!(
        "printf '%s\\n%s' \"${{CLAUDE_CONFIG_DIR:-unset}}\" \
         \"${{CLAUDE_SECURESTORAGE_CONFIG_DIR:-unset}}\" \
         > \"{root}/seen-$CORETEMPO_AGENT_ID.tmp\"\n\
         mv \"{root}/seen-$CORETEMPO_AGENT_ID.tmp\" \"{root}/seen-$CORETEMPO_AGENT_ID\"\n\
         printf '> '\nsleep 300\n",
        root = root.display()
    ));
    scaffold.write_workflow(&format!(
        "[agents.iso]\ndir = \"{dir}\"\nprompt = \"You are isolated.\"\n\
         isolated_config = true\nskills = [\"./skills/handoff\"]\n\
         [agents.plain]\ndir = \"{dir}\"\nprompt = \"You inherit.\"\n",
        dir = scaffold.agent_dir.display(),
    ));
    scaffold
}

/// The fake `claude` writes `seen-<agent>.tmp` and renames it, so a file that
/// exists is complete.
fn wait_for_file(path: &std::path::Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            return text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{} never appeared", path.display());
}

const GRANT: RunOptions = RunOptions {
    ephemeral_port: false,
    repoint_current: false,
    cleanup_run_dir: true,
    trust: TrustPolicy { grant: true },
};

#[tokio::test(flavor = "multi_thread")]
async fn isolated_agent_gets_a_seeded_dir_and_the_plain_one_does_not() {
    let scaffold = scaffold("iso-seeded").await;
    let (root, home) = (&scaffold.root, &scaffold.home);

    let run = scaffold.start(GRANT).await.unwrap();

    let dir = scaffold.run_dir(&run).join("claude-config-iso");
    assert_eq!(
        wait_for_file(&root.join("seen-iso")),
        format!("{}\n{}", dir.display(), home.join(".claude").display()),
        "the isolated agent keeps its login in the operator's store"
    );
    assert_eq!(wait_for_file(&root.join("seen-plain")), "unset\nunset");

    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert_eq!(
        std::fs::read_to_string(dir.join("settings.json")).unwrap(),
        SETTINGS_JSON
    );
    assert!(
        !dir.join(".credentials.json").exists(),
        "no credentials file or link in the managed dir: Claude Code replaces a \
         symlink on refresh (temp+rename), which strands every other holder"
    );
    assert_eq!(
        std::fs::read_link(dir.join("skills/handoff")).unwrap(),
        root.join("skills").join("handoff")
    );
    // The gate mirrored the operator's (policy-granted) trust before the spawn,
    // on top of the onboarding seed.
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(doc["hasCompletedOnboarding"], true);
    let root_key = trust_root(&scaffold.agent_dir).display().to_string();
    assert_eq!(doc["projects"][&root_key]["hasTrustDialogAccepted"], true);
    assert!(
        TrustStore::at(home.join(".claude.json"))
            .untrusted_roots([scaffold.agent_dir.as_path()])
            .unwrap()
            .is_empty(),
        "the operator store was granted by the preflight, as before"
    );

    run.stop().await.unwrap();
    assert!(!dir.exists(), "cleanup_run_dir removed the managed dir");
    assert!(
        home.join(".claude").join(".credentials.json").is_file(),
        "cleanup never touched the operator's file"
    );
    assert!(root.join("skills/handoff/SKILL.md").is_file());
}

/// An operator who relocates their own Claude config keeps it for the plain
/// agent (that *is* inheriting the operator's setup) and it becomes the
/// credential store the isolated agent shares.
#[tokio::test(flavor = "multi_thread")]
async fn an_operator_exported_config_dir_reaches_plain_and_is_the_credential_store() {
    let scaffold = scaffold("iso-opcfg").await;
    let operator_cfg = scaffold.home.join("relocated-claude");
    std::fs::create_dir_all(&operator_cfg).unwrap();
    // SAFETY: the scaffold holds the env lock; set after its own env writes
    // and removed before it drops.
    unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &operator_cfg) };

    let run = scaffold.start(GRANT).await.unwrap();
    let dir = scaffold.run_dir(&run).join("claude-config-iso");
    let seen_iso = wait_for_file(&scaffold.root.join("seen-iso"));
    let seen_plain = wait_for_file(&scaffold.root.join("seen-plain"));
    run.stop().await.unwrap();
    // SAFETY: as above.
    unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };

    assert_eq!(
        seen_plain,
        format!("{}\nunset", operator_cfg.display()),
        "plain inherits the operator's relocation untouched"
    );
    assert_eq!(
        seen_iso,
        format!("{}\n{}", dir.display(), operator_cfg.display()),
        "iso's credential store follows the operator's config dir"
    );
    assert!(
        operator_cfg.join(".claude.json").is_file(),
        "the trust preflight granted into the relocated store"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_dir_refuses_the_run_before_any_managed_dir_exists() {
    let scaffold = scaffold("iso-refuse").await;

    let err = scaffold
        .start(RunOptions {
            trust: TrustPolicy { grant: false },
            ..GRANT
        })
        .await
        .unwrap_err();

    assert!(matches!(err, RunError::Trust(_)), "{err:?}");
    assert!(
        !scaffold.home.join(".coretempo").exists(),
        "nothing under ~/.coretempo was created"
    );
}
