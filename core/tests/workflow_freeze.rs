#![expect(clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

use coretempo_core::types::AgentId;
use coretempo_core::types::config::PermissionPrompt;
use coretempo_core::workflow::{ConfigError, load_workflow};

fn write_temp(name: &str, contents: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("coretempo-freeze-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tempo.toml");
    std::fs::write(&path, contents).unwrap();
    path
}

/// Writes a sibling of a tempo.toml already placed by [`write_temp`].
fn write_beside(toml_path: &std::path::Path, name: &str, contents: &str) {
    std::fs::write(toml_path.parent().unwrap().join(name), contents).unwrap();
}

const BASE: &str = "[workflow]\nname = \"dev\"\nask_timeout_minutes = 30\n\
                    idle_debounce_seconds = 2.0\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n";

/// A webhook flow with an `ask` kickoff — the only shape an output contract is
/// allowed on. `[flows.hook.output]` sections append to it.
const HOOK: &str = "[flows.hook]\nagents = [\"a\"]\n\
                    trigger = { type = \"webhook\", edge = { to = \"a\", kind = \"ask\" } }\n";

#[test]
fn freezes_name_hash_and_durations() {
    let path = write_temp("base", BASE);
    let (file, frozen) = load_workflow(&path).unwrap();
    assert_eq!(file.workflow.name, "dev");
    assert_eq!(frozen.name, "dev");
    assert_eq!(frozen.hash.len(), 64);
    assert!(
        frozen
            .hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    assert_eq!(frozen.ask_timeout, Duration::from_mins(30));
    assert_eq!(frozen.idle_debounce, Duration::from_secs_f64(2.0));
    assert_eq!(frozen.scrollback, 5_000);
    assert!(frozen.source_path.is_absolute());
    assert!(
        frozen.flows.is_empty(),
        "no [flows] freezes to an empty map"
    );
    // deterministic: loading again yields the same hash
    let (_, frozen2) = load_workflow(&path).unwrap();
    assert_eq!(frozen.hash, frozen2.hash);
}

#[test]
fn hash_tracks_file_bytes() {
    let path = write_temp("hash", BASE);
    let (_, before) = load_workflow(&path).unwrap();
    std::fs::write(&path, format!("{BASE}# trailing comment\n")).unwrap();
    let (_, after) = load_workflow(&path).unwrap();
    assert_ne!(before.hash, after.hash);
}

#[test]
fn missing_file_is_io_error_with_path() {
    let err = load_workflow(std::path::Path::new("/nonexistent/tempo.toml")).unwrap_err();
    match err {
        ConfigError::Io { path, .. } => assert_eq!(path, PathBuf::from("/nonexistent/tempo.toml")),
        other => panic!("expected Io, got {other:?}"),
    }
}

#[test]
fn invalid_file_is_invalid_error_with_issues() {
    let path = write_temp("invalid", "[workflow]\nname = \"\"\n[agents]\n");
    let err = load_workflow(&path).unwrap_err();
    match err {
        ConfigError::Invalid { issues, summary } => {
            assert!(issues.len() >= 2);
            assert!(summary.contains("workflow.name"));
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn unreadable_schema_file_is_a_load_error_with_the_path() {
    let text = format!("{BASE}{HOOK}[flows.hook.output]\nschema_file = \"missing.json\"\n");
    let path = write_temp("schema-file-missing", &text);
    let err = load_workflow(&path).unwrap_err();
    assert!(err.to_string().contains("missing.json"), "{err}");
    match err {
        ConfigError::Invalid { issues, .. } => {
            assert_eq!(issues[0].path, "flows.hook.output");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn unparseable_schema_file_is_a_load_error() {
    let text = format!("{BASE}{HOOK}[flows.hook.output]\nschema_file = \"out.json\"\n");
    let path = write_temp("schema-file-bad-json", &text);
    write_beside(&path, "out.json", "{not json");
    let err = load_workflow(&path).unwrap_err();
    assert!(err.to_string().contains("out.json"), "{err}");
    assert!(err.to_string().contains("valid JSON"), "{err}");
}

#[test]
fn an_uncompilable_inline_schema_is_a_load_error() {
    let text = format!("{BASE}{HOOK}[flows.hook.output]\nschema = {{ type = \"nonsense\" }}\n");
    let path = write_temp("schema-uncompilable", &text);
    let err = load_workflow(&path).unwrap_err();
    assert!(err.to_string().contains("flows.hook.output"), "{err}");
}

/// HOME is process-global; every env-dependent assertion lives in this one test.
#[test]
fn tilde_dirs_expand_against_home() {
    let home = std::env::temp_dir().join(format!("coretempo-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: this is the only test in the workspace that mutates HOME, and it makes
    // no other env-dependent assertions concurrently.
    unsafe { std::env::set_var("HOME", &home) };
    let text = "[workflow]\nname = \"dev\"\n[agents.a]\ndir = \"~/proj\"\nprompt = \"p\"\n";
    let path = write_temp("tilde", text);
    let (_, frozen) = load_workflow(&path).unwrap();
    let a = frozen.agents.values().next().unwrap();
    assert_eq!(a.dir, home.join("proj"));
}

const TWO_FLOWS: &str = "[flows.classify]\nagents = [\"a\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"a\", kind = \"ask\" } }\n\
    [flows.classify.output]\nschema_file = \"classify.json\"\n\
    [flows.post]\nagents = [\"a\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"a\", kind = \"ask\" } }\n\
    [flows.post.output]\nschema_file = \"post.json\"\n";

#[test]
fn flows_freeze_with_members_and_contracts() {
    use coretempo_core::types::FlowName;
    let path = write_temp("flows-freeze", &format!("{BASE}{TWO_FLOWS}"));
    write_beside(&path, "classify.json", r#"{"type":"object"}"#);
    write_beside(&path, "post.json", r#"{"type":"object","required":["x"]}"#);
    let (_, frozen) = load_workflow(&path).unwrap();
    assert_eq!(frozen.flows.len(), 2);
    let post = &frozen.flows[&FlowName("post".into())];
    assert_eq!(
        post.members
            .iter()
            .map(|a| a.0.as_str())
            .collect::<Vec<_>>(),
        ["a"]
    );
    assert_eq!(post.edge.to.0, "a");
    assert!(post.message.is_none());
    let contract = post.output.as_ref().unwrap();
    assert_eq!(contract.target.0, "a");
    assert!(contract.check(r#"{"x":1}"#).is_ok());
    assert!(contract.check("{}").is_err());
}

#[test]
fn every_flows_schema_file_joins_the_hash() {
    let path = write_temp("flows-hash", &format!("{BASE}{TWO_FLOWS}"));
    write_beside(&path, "classify.json", r#"{"type":"object"}"#);
    write_beside(&path, "post.json", r#"{"type":"object"}"#);
    let (_, before) = load_workflow(&path).unwrap();
    // Editing the SECOND flow's schema (in name order) must move the hash too.
    write_beside(&path, "post.json", r#"{"type":"object","required":["x"]}"#);
    let (_, after) = load_workflow(&path).unwrap();
    assert_ne!(
        before.hash, after.hash,
        "post.json edit must change the hash"
    );
    write_beside(&path, "post.json", r#"{"type":"object"}"#);
    write_beside(
        &path,
        "classify.json",
        r#"{"type":"object","required":["y"]}"#,
    );
    let (_, after2) = load_workflow(&path).unwrap();
    assert_ne!(
        before.hash, after2.hash,
        "classify.json edit must change the hash"
    );
}

/// Two layouts whose schema files concatenate to the same bytes: a trailing
/// space on the first flow's file, or the same space leading the second's.
/// Raw concatenation hashes them identically, so an edit that only moves a byte
/// across the boundary would leave the freeze hash — the thing serve mode's
/// `workflow_changed` refusal reads — unmoved.
#[test]
fn adjacent_schema_files_cannot_alias_across_the_boundary() {
    let path = write_temp("flows-hash-frame", &format!("{BASE}{TWO_FLOWS}"));
    write_beside(&path, "classify.json", "{\"type\":\"object\"} ");
    write_beside(&path, "post.json", "{}");
    let (_, before) = load_workflow(&path).unwrap();
    write_beside(&path, "classify.json", "{\"type\":\"object\"}");
    write_beside(&path, "post.json", " {}");
    let (_, after) = load_workflow(&path).unwrap();
    assert_ne!(
        before.hash, after.hash,
        "each schema file's bytes must be framed, not run together"
    );
}

#[test]
fn for_flow_derives_the_member_subset() {
    use coretempo_core::types::FlowName;
    // Two agents; the flow spans only `reader`.
    let text = "[workflow]\nname = \"dev\"\n\
        [agents.reader]\ndir = \"/tmp\"\nprompt = \"p\"\n\
        [agents.writer]\ndir = \"/tmp\"\nprompt = \"p\"\n\
        [flows.solo]\nagents = [\"reader\"]\n\
        trigger = { type = \"webhook\", edge = { to = \"reader\", kind = \"ask\" } }\n";
    let path = write_temp("for-flow", text);
    let (_, frozen) = load_workflow(&path).unwrap();
    let derived = frozen.for_flow(&FlowName("solo".into())).unwrap();
    assert_eq!(
        derived
            .agents
            .keys()
            .map(|a| a.0.as_str())
            .collect::<Vec<_>>(),
        ["reader"],
        "agents map is the member set"
    );
    assert_eq!(derived.flows.len(), 1, "flows map is just this flow");
    assert_eq!(derived.hash, frozen.hash, "hash unchanged");
    assert_eq!(derived.source_path, frozen.source_path, "source unchanged");
    // Spec §2: a subset run's prompts never name non-spawned teammates.
    let prompt = derived
        .system_prompt(&coretempo_core::types::AgentId("reader".into()))
        .unwrap();
    assert!(prompt.contains("Other agents: (none)"), "{prompt}");
    assert!(frozen.for_flow(&FlowName("nope".into())).is_none());
}

const ISO: &str = "[agents.iso]\ndir = \"/tmp\"\nprompt = \"p\"\nisolated_config = true\n\
                   skills = [\"./skills/handoff\"]\n";

fn write_skill(toml_path: &std::path::Path, rel: &str, body: &str) -> PathBuf {
    let dir = toml_path.parent().unwrap().join(rel);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
    dir
}

#[test]
fn skills_freeze_to_absolute_paths_beside_the_toml() {
    let path = write_temp("skills-resolve", &format!("{BASE}{ISO}"));
    let dir = write_skill(&path, "skills/handoff", "---\nname: handoff\n---\n");
    let (_, frozen) = load_workflow(&path).unwrap();
    let iso = &frozen.agents[&coretempo_core::types::AgentId("iso".into())];
    assert_eq!(iso.skills, vec![dir]);
}

#[test]
fn a_skill_dir_that_does_not_exist_fails_the_load() {
    let path = write_temp("skills-missing", &format!("{BASE}{ISO}"));
    let err = load_workflow(&path).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("agents.iso.skills[0]"), "{text}");
    assert!(text.contains("is not a directory"), "{text}");
    assert!(
        text.contains(&path.parent().unwrap().display().to_string()),
        "names the base dir: {text}"
    );
}

#[test]
fn a_skill_dir_without_skill_md_fails_the_load() {
    let path = write_temp("skills-no-md", &format!("{BASE}{ISO}"));
    std::fs::create_dir_all(path.parent().unwrap().join("skills/handoff")).unwrap();
    let err = load_workflow(&path).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("agents.iso.skills[0]"), "{text}");
    assert!(text.contains("has no SKILL.md"), "{text}");
}

#[test]
fn skill_bytes_join_the_hash() {
    let path = write_temp("skills-hash", &format!("{BASE}{ISO}"));
    let dir = write_skill(&path, "skills/handoff", "v1");
    let (_, before) = load_workflow(&path).unwrap();

    std::fs::write(dir.join("SKILL.md"), "v2").unwrap();
    let (_, edited) = load_workflow(&path).unwrap();
    assert_ne!(before.hash, edited.hash, "editing SKILL.md moves the hash");

    std::fs::write(dir.join("SKILL.md"), "v1").unwrap();
    std::fs::create_dir_all(dir.join("references")).unwrap();
    std::fs::write(dir.join("references/notes.md"), "x").unwrap();
    let (_, added) = load_workflow(&path).unwrap();
    assert_ne!(
        before.hash, added.hash,
        "adding a nested file moves the hash"
    );

    std::fs::remove_file(dir.join("references/notes.md")).unwrap();
    std::fs::write(path.parent().unwrap().join("skills/unrelated.md"), "y").unwrap();
    let (_, sibling) = load_workflow(&path).unwrap();
    assert_eq!(
        before.hash, sibling.hash,
        "a sibling outside the skill dir does not"
    );
}

#[test]
fn moving_identical_bytes_to_another_path_moves_the_hash() {
    let path = write_temp("skills-rename", &format!("{BASE}{ISO}"));
    let dir = write_skill(&path, "skills/handoff", "v1");
    let _ = std::fs::remove_file(dir.join("b.md"));
    std::fs::write(dir.join("a.md"), "same").unwrap();
    let (_, before) = load_workflow(&path).unwrap();

    std::fs::rename(dir.join("a.md"), dir.join("b.md")).unwrap();
    let (_, renamed) = load_workflow(&path).unwrap();
    assert_ne!(
        before.hash, renamed.hash,
        "the relative path is framed, so a rename with identical bytes is a change"
    );
}

#[test]
fn two_agents_declaring_one_skill_hash_it_under_each_id() {
    const ISO2: &str = "[agents.iso2]\ndir = \"/tmp\"\nprompt = \"p\"\nisolated_config = true\n\
                        skills = [\"./skills/handoff\"]\n";
    const PLAIN2: &str = "[agents.iso2]\ndir = \"/tmp\"\nprompt = \"p\"\n";
    let shared = write_temp("skills-shared", &format!("{BASE}{ISO}{ISO2}"));
    let dir = write_skill(&shared, "skills/handoff", "v1");
    let (_, both) = load_workflow(&shared).unwrap();
    let (_, both_again) = load_workflow(&shared).unwrap();
    assert_eq!(both.hash, both_again.hash, "deterministic");

    std::fs::write(&shared, format!("{BASE}{ISO}{PLAIN2}")).unwrap();
    let (_, one) = load_workflow(&shared).unwrap();
    assert_ne!(
        both.hash, one.hash,
        "the second declaration adds its own frames, so dropping it is a change"
    );

    std::fs::write(&shared, format!("{BASE}{ISO}{ISO2}")).unwrap();
    std::fs::write(dir.join("SKILL.md"), "v2").unwrap();
    let (_, edited) = load_workflow(&shared).unwrap();
    assert_ne!(both.hash, edited.hash, "both declarations see the edit");
}

#[test]
fn an_unreadable_file_inside_a_skill_dir_is_a_skill_io_error_naming_it() {
    use std::os::unix::fs::PermissionsExt;
    let path = write_temp("skills-unreadable", &format!("{BASE}{ISO}"));
    let dir = write_skill(&path, "skills/handoff", "v1");
    let secret = dir.join("secret.md");
    std::fs::write(&secret, "x").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = load_workflow(&path);
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = result.unwrap_err();
    assert!(matches!(err, ConfigError::SkillIo { .. }), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("agents.iso.skills"), "{text}");
    assert!(
        text.contains(&secret.display().to_string()),
        "names the file, not the dir: {text}"
    );
}

#[test]
fn an_unreadable_parent_of_a_skill_dir_is_a_skill_io_error_not_a_missing_dir() {
    use std::os::unix::fs::PermissionsExt;
    const LOCKED: &str = "[agents.iso]\ndir = \"/tmp\"\nprompt = \"p\"\nisolated_config = true\n\
                          skills = [\"./locked/handoff\"]\n";
    let path = write_temp("skills-locked", &format!("{BASE}{LOCKED}"));
    let dir = write_skill(&path, "locked/handoff", "v1");
    let locked = dir.parent().unwrap();
    std::fs::set_permissions(locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = load_workflow(&path);
    std::fs::set_permissions(locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = result.unwrap_err();
    assert!(matches!(err, ConfigError::SkillIo { .. }), "{err:?}");
    let text = err.to_string();
    assert!(text.contains(&dir.display().to_string()), "{text}");
    assert!(
        text.to_lowercase().contains("permission denied"),
        "the OS reason is kept, not reported as 'not a directory': {text}"
    );
}

#[test]
fn a_skill_entry_that_cannot_be_stat_ed_is_named_itself() {
    use std::os::unix::fs::PermissionsExt;
    let path = write_temp("skills-nostat", &format!("{BASE}{ISO}"));
    let dir = write_skill(&path, "skills/handoff", "v1");
    let sub = dir.join("references");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("notes.md"), "x").unwrap();
    // Readable but not searchable: the listing succeeds, stat of each child does not.
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o444)).unwrap();
    let result = load_workflow(&path);
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = result.unwrap_err();
    assert!(matches!(err, ConfigError::SkillIo { .. }), "{err:?}");
    let text = err.to_string();
    assert!(
        text.contains(&sub.join("notes.md").display().to_string()),
        "names the entry, not its directory: {text}"
    );
}

#[test]
fn a_symlink_inside_a_skill_dir_fails_the_load() {
    let path = write_temp("skills-symlink", &format!("{BASE}{ISO}"));
    let dir = write_skill(&path, "skills/handoff", "v1");
    // `write_temp` reuses `/tmp/coretempo-freeze-<pid>-<name>` without cleanup.
    let _ = std::fs::remove_file(dir.join("leak"));
    std::os::unix::fs::symlink(&path, dir.join("leak")).unwrap();
    let err = load_workflow(&path).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("agents.iso.skills"), "{text}");
    assert!(text.contains("leak"), "{text}");
    assert!(text.contains("not a regular file or directory"), "{text}");
}

#[test]
fn on_permission_prompt_defaults_to_deny() {
    let path = write_temp("perm-default", BASE);
    let (file, _) = load_workflow(&path).unwrap();
    assert_eq!(
        file.agents[&AgentId("a".into())].on_permission_prompt,
        PermissionPrompt::Deny
    );
}

#[test]
fn on_permission_prompt_wait_is_accepted() {
    let path = write_temp(
        "perm-wait",
        &format!("{BASE}on_permission_prompt = \"wait\"\n"),
    );
    let (file, _) = load_workflow(&path).unwrap();
    assert_eq!(
        file.agents[&AgentId("a".into())].on_permission_prompt,
        PermissionPrompt::Wait
    );
}

#[test]
fn on_permission_prompt_rejects_other_words_naming_the_valid_ones() {
    let path = write_temp(
        "perm-bad",
        &format!("{BASE}on_permission_prompt = \"ask\"\n"),
    );
    let err = load_workflow(&path).unwrap_err().to_string();
    assert!(err.contains("deny") && err.contains("wait"), "{err}");
}
