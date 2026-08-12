#![expect(clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Duration;

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

/// A webhook trigger with an `ask` kickoff — the only shape `[trigger.output]`
/// is allowed on.
const WEBHOOK: &str = "[trigger]\ntype = \"webhook\"\nedge = { to = \"a\", kind = \"ask\" }\n";

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
fn schema_file_bytes_join_the_freeze_hash() {
    let text = format!("{BASE}{WEBHOOK}[trigger.output]\nschema_file = \"out.json\"\n");
    let path = write_temp("schema-file-hash", &text);
    write_beside(&path, "out.json", r#"{"type":"object"}"#);
    let (_, before) = load_workflow(&path).unwrap();
    write_beside(&path, "out.json", r#"{"type":"object","required":["x"]}"#);
    let (_, after) = load_workflow(&path).unwrap();
    assert_ne!(
        before.hash, after.hash,
        "a schema_file edit must change the freeze hash"
    );
    let contract = after.output.as_ref().unwrap();
    assert_eq!(contract.target.0, "a");
    assert_eq!(contract.max_repairs, 2, "default");
    assert!(contract.check(r#"{"x":1}"#).is_ok());
    assert!(contract.check("{}").is_err());
}

#[test]
fn unreadable_schema_file_is_a_load_error_with_the_path() {
    let text = format!("{BASE}{WEBHOOK}[trigger.output]\nschema_file = \"missing.json\"\n");
    let path = write_temp("schema-file-missing", &text);
    let err = load_workflow(&path).unwrap_err();
    assert!(err.to_string().contains("missing.json"), "{err}");
    match err {
        ConfigError::Invalid { issues, .. } => {
            assert_eq!(issues[0].path, "trigger.output");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn unparseable_schema_file_is_a_load_error() {
    let text = format!("{BASE}{WEBHOOK}[trigger.output]\nschema_file = \"out.json\"\n");
    let path = write_temp("schema-file-bad-json", &text);
    write_beside(&path, "out.json", "{not json");
    let err = load_workflow(&path).unwrap_err();
    assert!(err.to_string().contains("out.json"), "{err}");
    assert!(err.to_string().contains("valid JSON"), "{err}");
}

#[test]
fn inline_schema_compiles_onto_the_frozen_workflow() {
    let text = format!(
        "{BASE}{WEBHOOK}[trigger.output]\n\
         schema = {{ type = \"object\", required = [\"name\"] }}\nmax_repairs = 0\n"
    );
    let path = write_temp("schema-inline", &text);
    let (_, frozen) = load_workflow(&path).unwrap();
    let contract = frozen.output.as_ref().unwrap();
    assert_eq!(contract.max_repairs, 0);
    assert!(contract.check(r#"{"name":"x"}"#).is_ok());
    assert!(contract.check("{}").is_err());
}

#[test]
fn an_uncompilable_inline_schema_is_a_load_error() {
    let text = format!("{BASE}{WEBHOOK}[trigger.output]\nschema = {{ type = \"nonsense\" }}\n");
    let path = write_temp("schema-uncompilable", &text);
    let err = load_workflow(&path).unwrap_err();
    assert!(err.to_string().contains("trigger.output"), "{err}");
}

#[test]
fn no_output_section_leaves_the_contract_unset() {
    let path = write_temp("schema-absent", &format!("{BASE}{WEBHOOK}"));
    let (_, frozen) = load_workflow(&path).unwrap();
    assert!(frozen.output.is_none());
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
